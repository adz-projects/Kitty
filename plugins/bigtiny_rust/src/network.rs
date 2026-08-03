use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use ipnet::IpNet;
use once_cell::sync::Lazy;
use reqwest::Client;
use tokio::sync::Mutex;

pub static TAILSCALE_NETWORK: Lazy<IpNet> =
    Lazy::new(|| IpNet::new(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 0)), 10).unwrap());
pub const TAILSCALE_LOCAL_API: &str = "http://[::1]:42711/localapi/v0/status?peers=1";
pub const DIRECT_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct TailscalePeer {
    pub name: String,
    pub tailscale_ip: String,
    pub direct_ips: Vec<String>,
}

pub struct TailscaleClient {
    peers_cache: Mutex<Option<HashMap<String, TailscalePeer>>>,
    resolved_cache: DashMap<String, Option<String>>,
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
        if cache.is_some() {
            return cache.clone().unwrap_or_default();
        }
        let peers = self.fetch_peers().await;
        *cache = Some(peers.clone());
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
        self.resolved_cache.insert(host_or_ip.to_string(), direct_ip);
    }

    pub async fn resolve_direct_ip(&self, host_or_ip: &str) -> Option<String> {
        if let Some(cached) = self.resolved_cache.get(host_or_ip) {
            return cached.value().clone();
        }

        let peers = self.get_peers().await;
        let peer = peers.get(host_or_ip)?;

        let direct_ip = Self::resolve_dns_excluding_tailscale(&peer.name).await;
        self.resolved_cache
            .insert(host_or_ip.to_string(), direct_ip.clone());
        direct_ip
    }

    async fn resolve_dns_excluding_tailscale(hostname: &str) -> Option<String> {
        match tokio::net::lookup_host((hostname, 0)).await {
            Ok(addrs) => {
                for addr in addrs {
                    if !TAILSCALE_NETWORK.contains(&addr.ip()) {
                        return Some(addr.ip().to_string());
                    }
                }
                None
            }
            Err(_) => None,
        }
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
}
