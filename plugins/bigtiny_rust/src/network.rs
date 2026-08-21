use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use ipnet::IpNet;
use once_cell::sync::Lazy;
use reqwest::Client;
use tokio::sync::Mutex;

pub static TAILSCALE_NETWORK: Lazy<IpNet> =
    Lazy::new(|| IpNet::new(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 0)), 10).unwrap());
pub const TAILSCALE_LOCAL_API: &str = "http://[::1]:42711/localapi/v0/status?peers=1";
pub const DIRECT_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a fetched peer map is trusted before re-querying the local
/// Tailscale API — a network change must not leave the daemon dialing stale
/// direct IPs forever (see #9).
const PEERS_CACHE_TTL: Duration = Duration::from_secs(600);
/// How long a resolved direct address is trusted before re-resolving (and
/// re-checking it against the current peer map) — see #9.
const RESOLVED_CACHE_TTL: Duration = Duration::from_secs(300);
/// Ceiling for a single `lookup_host` call — a stuck system resolver used to
/// delay every Tailscale-provider request indefinitely (see #8).
const DNS_RESOLVE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct TailscalePeer {
    pub name: String,
    pub tailscale_ip: String,
    pub direct_ips: Vec<String>,
}

pub struct TailscaleClient {
    peers_cache: Mutex<Option<(Instant, HashMap<String, TailscalePeer>)>>,
    resolved_cache: DashMap<String, (Instant, Option<String>)>,
    warned_unavailable: AtomicBool,
    client: Client,
}

impl Default for TailscaleClient {
    fn default() -> Self {
        Self::new()
    }
}

impl TailscaleClient {
    pub fn new() -> Self {
        Self {
            peers_cache: Mutex::new(None),
            resolved_cache: DashMap::new(),
            warned_unavailable: AtomicBool::new(false),
            client: Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap_or_default(),
        }
    }

    pub async fn get_peers(&self) -> HashMap<String, TailscalePeer> {
        let mut cache = self.peers_cache.lock().await;
        // TTL instead of never-expiring (see #9): a cached peer map older
        // than `PEERS_CACHE_TTL` is discarded and re-fetched, so a changed
        // network (peer re-IP'd, gone, etc.) is picked up without a restart.
        if let Some((fetched_at, peers)) = cache.as_ref() {
            if fetched_at.elapsed() < PEERS_CACHE_TTL {
                return peers.clone();
            }
        }
        let peers = self.fetch_peers().await;
        *cache = Some((Instant::now(), peers.clone()));
        peers
    }

    async fn fetch_peers(&self) -> HashMap<String, TailscalePeer> {
        match self.client.get(TAILSCALE_LOCAL_API).send().await {
            Ok(response) => match response.error_for_status() {
                Ok(response) => {
                    let data: serde_json::Value = match response.json().await {
                        Ok(d) => d,
                        Err(_) => return HashMap::new(),
                    };
                    Self::parse_peers(&data)
                }
                Err(_) => {
                    self.warn_if_first();
                    HashMap::new()
                }
            },
            Err(_) => {
                self.warn_if_first();
                HashMap::new()
            }
        }
    }

    fn warn_if_first(&self) {
        if !self.warned_unavailable.swap(true, Ordering::SeqCst) {
            tracing::warn!(
                "Tailscale local API not reachable — direct-address routing disabled \
                 for Tailscale peers (this is a no-op if you don't use Tailscale)."
            );
        }
    }

    fn parse_peers(data: &serde_json::Value) -> HashMap<String, TailscalePeer> {
        let mut peers = HashMap::new();
        if let Some(peer_map) = data.get("Peer").and_then(|p| p.as_object()) {
            for peer in peer_map.values() {
                let name = peer
                    .get("DNSName")
                    .and_then(|n| n.as_str())
                    .map(|s| s.trim_end_matches('.').to_string());
                let addrs = peer
                    .get("TailscaleIPs")
                    .and_then(|a| a.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|a| a.as_str())
                            .map(String::from)
                            .collect::<Vec<_>>()
                    });

                if let (Some(name), Some(addrs)) = (name, addrs) {
                    if name.is_empty() || addrs.is_empty() {
                        continue;
                    }
                    let parsed = TailscalePeer {
                        name: name.clone(),
                        tailscale_ip: addrs[0].clone(),
                        direct_ips: addrs.clone(),
                    };
                    for ip in &addrs {
                        peers.insert(ip.clone(), parsed.clone());
                    }
                }
            }
        }
        peers
    }

    /// Test-only seam: inject a resolved (or explicitly unresolved) direct IP
    /// for a host without ever touching the real local Tailscale API or DNS —
    /// `resolve_direct_ip` checks `resolved_cache` first (see below) and
    /// returns immediately on a hit, so seeding this cache directly is enough
    /// to exercise `maybe_direct_url`'s full behavior in a unit test.
    #[cfg(test)]
    pub(crate) fn seed_resolved_for_test(&self, host_or_ip: &str, direct_ip: Option<String>) {
        self.resolved_cache
            .insert(host_or_ip.to_string(), (Instant::now(), direct_ip));
    }

    pub async fn resolve_direct_ip(&self, host_or_ip: &str) -> Option<String> {
        // TTL instead of never-expiring (see #9): a resolved address older
        // than `RESOLVED_CACHE_TTL` is re-resolved against a fresh peer map,
        // so a peer that changed IPs after a network change stops being
        // dialed at its old address.
        if let Some(cached) = self.resolved_cache.get(host_or_ip) {
            if cached.value().0.elapsed() < RESOLVED_CACHE_TTL {
                return cached.value().1.clone();
            }
        }

        let peers = self.get_peers().await;
        let Some(peer) = peers.get(host_or_ip) else {
            // The peer is gone from the current map — don't keep a stale
            // resolved address for it any longer.
            self.resolved_cache.remove(host_or_ip);
            return None;
        };

        let direct_ip = Self::resolve_dns_excluding_tailscale(&peer.name).await;
        self.resolved_cache.insert(
            host_or_ip.to_string(),
            (Instant::now(), direct_ip.clone()),
        );
        direct_ip
    }
}

/// Prefer the first IPv4 address outside the Tailscale CGNAT range, falling
/// back to an IPv6 one if that's all there is — see
/// `resolve_dns_excluding_tailscale`. Split out so the ordering policy is
/// testable without touching OS-level DNS.
fn pick_direct_address(addrs: impl Iterator<Item = std::net::SocketAddr>) -> Option<String> {
    let mut v6_fallback: Option<String> = None;
    for addr in addrs {
        if !TAILSCALE_NETWORK.contains(&addr.ip()) {
            if addr.is_ipv4() {
                return Some(addr.ip().to_string());
            }
            if v6_fallback.is_none() {
                v6_fallback = Some(addr.ip().to_string());
            }
        }
    }
    v6_fallback
}

impl TailscaleClient {
    /// `lookup_host` with a hard timeout and IPv4-first ordering — the two
    /// #8 fixes. A stuck system resolver used to delay every
    /// Tailscale-provider request indefinitely (there was no timeout at all),
    /// and the *first* non-Tailscale result (frequently IPv6-only on Android)
    /// was used verbatim even when unreachable. The direct-LAN shortcut is an
    /// optimization, not a requirement — the tunnel remains the authoritative
    /// path, so a `None` here just means "skip the direct attempt".
    async fn resolve_dns_excluding_tailscale(hostname: &str) -> Option<String> {
        let lookup = async {
            match tokio::net::lookup_host((hostname, 0)).await {
                Ok(addrs) => pick_direct_address(addrs),
                Err(_) => None,
            }
        };
        // A stuck resolver (or any lookup failure) resolves to `None` —
        // skip the direct attempt, let the tunnel carry the request.
        tokio::time::timeout(DNS_RESOLVE_TIMEOUT, lookup)
            .await
            .unwrap_or_default()
    }
}

pub fn is_tailscale_ip(host: &str) -> bool {
    if let Ok(ip) = host.parse::<IpAddr>() {
        TAILSCALE_NETWORK.contains(&ip)
    } else {
        false
    }
}

/// If `url`'s host is a Tailscale IP with a discoverable direct (LAN)
/// address, returns that same URL with the host swapped to the direct
/// address. Otherwise `None` — callers should just use the original URL.
/// Mirrors Python's `PreferDirectTransport`, minus the custom-transport
/// machinery: reqwest has no per-request transport hook, so instead of
/// wrapping the client, callers try this rewritten URL first (with a short
/// timeout) and fall back to the original on connect failure.
pub async fn maybe_direct_url(tailscale: &TailscaleClient, url: &str) -> Option<String> {
    let mut parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if matches!(host, "localhost" | "127.0.0.1" | "::1") || !is_tailscale_ip(host) {
        return None;
    }
    let direct_ip = tailscale.resolve_direct_ip(host).await?;
    parsed.set_host(Some(&direct_ip)).ok()?;
    Some(parsed.to_string())
}

/// Coverage for the LAN-shortcut mechanism behind `send_preferring_direct`
/// (a Tailscale-address provider transparently gets LAN speed at home and
/// Tailscale-tunnel reachability away from it, with no manual URL switching —
/// see `provider::openai_compat::OpenAICompatibleProvider::send_preferring_direct`
/// for the actual request-level fallback this backs).
///
/// Known gap: `resolve_dns_excluding_tailscale` itself (the real
/// `tokio::net::lookup_host` call) is not covered here — mocking OS-level DNS
/// resolution isn't practical from a unit test. `maybe_direct_url`'s tests
/// below cover everything around it via `TailscaleClient::seed_resolved_for_test`,
/// which seeds the cache `resolve_direct_ip` consults *before* ever reaching
/// that DNS call.
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn is_tailscale_ip_true_only_inside_the_cgnat_range() {
        assert!(is_tailscale_ip("100.64.0.1"));
        assert!(is_tailscale_ip("100.100.100.100"));
        assert!(is_tailscale_ip("100.127.255.255"));
        assert!(!is_tailscale_ip("100.63.255.255")); // just below the range
        assert!(!is_tailscale_ip("100.128.0.0")); // just above the range
        assert!(!is_tailscale_ip("192.168.1.50")); // ordinary LAN address
        assert!(!is_tailscale_ip("8.8.8.8")); // public address
        assert!(!is_tailscale_ip("myhost.example.com")); // not an IP at all
    }

    #[test]
    fn parse_peers_indexes_by_every_tailscale_ip_and_trims_trailing_dot() {
        let data = json!({
            "Peer": {
                "nodekey:abc": {
                    "DNSName": "llm-box.tailnet-1234.ts.net.",
                    "TailscaleIPs": ["100.64.1.2", "fd7a:115c:a1e0::1"],
                }
            }
        });
        let peers = TailscaleClient::parse_peers(&data);
        assert_eq!(peers.len(), 2, "indexed under both addresses");
        let by_v4 = peers.get("100.64.1.2").expect("v4 key present");
        assert_eq!(by_v4.name, "llm-box.tailnet-1234.ts.net"); // trailing dot trimmed
        assert_eq!(by_v4.tailscale_ip, "100.64.1.2"); // first address wins
        assert_eq!(by_v4.direct_ips, vec!["100.64.1.2", "fd7a:115c:a1e0::1"]);
        let by_v6 = peers.get("fd7a:115c:a1e0::1").expect("v6 key present");
        assert_eq!(by_v6.name, by_v4.name);
    }

    #[test]
    fn parse_peers_skips_entries_with_no_name_or_no_addresses() {
        let data = json!({
            "Peer": {
                "no-name": { "TailscaleIPs": ["100.64.1.2"] },
                "no-addrs": { "DNSName": "empty-box.ts.net." },
                "empty-name": { "DNSName": "", "TailscaleIPs": ["100.64.1.3"] },
                "empty-addrs": { "DNSName": "no-ip.ts.net.", "TailscaleIPs": [] },
            }
        });
        let peers = TailscaleClient::parse_peers(&data);
        assert!(peers.is_empty());
    }

    #[test]
    fn parse_peers_on_missing_or_malformed_peer_map_returns_empty() {
        assert!(TailscaleClient::parse_peers(&json!({})).is_empty());
        assert!(TailscaleClient::parse_peers(&json!({ "Peer": "not an object" })).is_empty());
    }

    #[tokio::test]
    async fn maybe_direct_url_is_none_for_loopback_hosts() {
        let ts = TailscaleClient::new();
        assert_eq!(maybe_direct_url(&ts, "http://localhost:8080/v1").await, None);
        assert_eq!(maybe_direct_url(&ts, "http://127.0.0.1:8080/v1").await, None);
        assert_eq!(maybe_direct_url(&ts, "http://[::1]:8080/v1").await, None);
    }

    #[tokio::test]
    async fn maybe_direct_url_is_none_for_a_non_tailscale_host() {
        let ts = TailscaleClient::new();
        // A LAN address is not in the Tailscale CGNAT range, so this must be
        // a no-op regardless of anything seeded in the resolved cache.
        ts.seed_resolved_for_test("192.168.1.50", Some("192.168.1.50".to_string()));
        assert_eq!(
            maybe_direct_url(&ts, "http://192.168.1.50:8080/v1").await,
            None
        );
    }

    #[tokio::test]
    async fn maybe_direct_url_is_none_when_no_direct_address_is_known() {
        let ts = TailscaleClient::new();
        ts.seed_resolved_for_test("100.64.1.2", None);
        assert_eq!(
            maybe_direct_url(&ts, "http://100.64.1.2:8080/v1").await,
            None
        );
    }

    #[tokio::test]
    async fn maybe_direct_url_rewrites_the_host_to_the_resolved_direct_address() {
        let ts = TailscaleClient::new();
        ts.seed_resolved_for_test("100.64.1.2", Some("192.168.1.50".to_string()));
        let rewritten = maybe_direct_url(&ts, "http://100.64.1.2:8080/v1/chat/completions")
            .await
            .expect("a direct address was seeded");
        assert_eq!(rewritten, "http://192.168.1.50:8080/v1/chat/completions");
    }

    /// #8: the direct address must prefer IPv4 even when an IPv6 result comes
    /// first from the resolver — an IPv6-only address is frequently
    /// unreachable on Android, and the LAN shortcut is an optimization.
    #[test]
    fn pick_direct_address_prefers_ipv4_over_an_earlier_ipv6_result() {
        use std::net::{IpAddr, Ipv6Addr, SocketAddr};
        let v6: SocketAddr = (Ipv6Addr::LOCALHOST, 8080).into();
        let v4: SocketAddr = ("192.168.1.50".parse::<IpAddr>().unwrap(), 8080).into();
        assert_eq!(
            pick_direct_address(vec![v6, v4].into_iter()).as_deref(),
            Some("192.168.1.50")
        );
    }

    #[test]
    fn pick_direct_address_skips_tailscale_cgnat_addresses() {
        use std::net::{IpAddr, SocketAddr};
        let cgnat: SocketAddr = ("100.64.5.5".parse::<IpAddr>().unwrap(), 8080).into();
        let lan: SocketAddr = ("10.0.0.9".parse::<IpAddr>().unwrap(), 8080).into();
        assert_eq!(
            pick_direct_address(vec![cgnat, lan].into_iter()).as_deref(),
            Some("10.0.0.9")
        );
    }

    #[test]
    fn pick_direct_address_falls_back_to_ipv6_when_no_ipv4_exists() {
        use std::net::{Ipv6Addr, SocketAddr};
        let v6: SocketAddr = (Ipv6Addr::LOCALHOST, 8080).into();
        assert_eq!(
            pick_direct_address(vec![v6].into_iter()).as_deref(),
            Some("::1")
        );
        assert_eq!(pick_direct_address(vec![].into_iter()), None);
    }

    /// #9: a resolved address older than `RESOLVED_CACHE_TTL` must not be
    /// returned blindly — it is re-resolved (here against an empty peer map,
    /// so it must come back `None`) instead of staying stale forever.
    #[tokio::test]
    async fn stale_resolved_cache_entries_are_re_resolved() {
        let ts = TailscaleClient::new();
        ts.resolved_cache.insert(
            "100.64.1.2".into(),
            (
                Instant::now() - RESOLVED_CACHE_TTL - Duration::from_secs(1),
                Some("10.0.0.9".into()),
            ),
        );
        assert_eq!(
            ts.resolve_direct_ip("100.64.1.2").await,
            None,
            "a stale resolved address must not be trusted past its TTL"
        );
    }

    /// #9: a fresh resolved address short-circuits without touching DNS.
    #[tokio::test]
    async fn fresh_resolved_cache_entries_short_circuit() {
        let ts = TailscaleClient::new();
        ts.seed_resolved_for_test("100.64.1.2", Some("192.168.1.50".into()));
        assert_eq!(
            ts.resolve_direct_ip("100.64.1.2").await.as_deref(),
            Some("192.168.1.50")
        );
    }
}
