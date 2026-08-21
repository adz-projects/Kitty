//! SSRF guard for `lean_web_scrape` (audit #109).
//!
//! The scrape tool fetches a model-supplied URL. Without validation that is
//! a textbook SSRF primitive: `http://127.0.0.1:<bigtiny-port>/...`,
//! `http://169.254.169.254/latest/meta-data/`, or a public URL that 302s to
//! either of those, would have its body read straight into model context.
//!
//! The policy: **http/https only**, and the host must resolve to *public*
//! IPs only — loopback, private (RFC-1918), link-local (incl. the cloud
//! metadata address), CGNAT, multicast, and reserved ranges are rejected.
//! The check runs before the initial request and again on every redirect hop
//! (see `scrape::web_scrape`'s `redirect::Policy::custom`), so a public page
//! cannot bounce the fetch somewhere internal.
//!
//! Two known limits, accepted deliberately:
//! - DNS is re-resolved by reqwest at connect time, so a hostname whose
//!   answer flips between the check and the connect (classic rebinding) can
//!   slip the window. Closing it would require pinning the connection to the
//!   validated address, which reqwest does not expose per-request.
//! - A hostname that fails to resolve here is let through so the request
//!   itself produces the usual `SCRAPE_NETWORK_ERROR` rather than a
//!   misleading "blocked" verdict; reqwest's own resolution then fails the
//!   same way.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

/// Validates `url` against the scrape fetch policy. `Err(reason)` describes
/// the rejection in one short sentence, safe to surface in the tool
/// envelope's `detail` field.
pub fn check_url(url: &url::Url) -> Result<(), String> {
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "scheme '{other}' is not allowed; only http and https URLs can be scraped"
            ));
        }
    }
    let Some(host) = url.host() else {
        return Err("URL has no host".to_string());
    };
    match host {
        url::Host::Ipv4(ip) => check_ip(IpAddr::V4(ip)),
        url::Host::Ipv6(ip) => check_ip(IpAddr::V6(ip)),
        url::Host::Domain(domain) => {
            // ToSocketAddrs needs a port; the value is irrelevant to the IP
            // set returned, so the URL's own (or the scheme default) is fine.
            let port = url.port_or_known_default().unwrap_or(80);
            let addrs: Vec<IpAddr> = match (domain, port).to_socket_addrs() {
                Ok(iter) => iter.map(|a| a.ip()).collect(),
                // Unresolvable here means unresolvable for the request too —
                // let the fetch produce the normal network error instead of
                // a bogus "blocked" verdict.
                Err(_) => return Ok(()),
            };
            if addrs.is_empty() {
                return Ok(());
            }
            // Strict: a hostname that round-robins between a public and a
            // non-public address is rejected outright.
            for ip in addrs {
                check_ip(ip)?;
            }
            Ok(())
        }
    }
}

fn check_ip(ip: IpAddr) -> Result<(), String> {
    if ip_is_public(ip) {
        Ok(())
    } else {
        Err(format!("address {ip} is not a public IP; loopback, private, link-local and reserved ranges are blocked"))
    }
}

/// True when `ip` is a public, routable unicast address. The inverse of the
/// non-public set, enumerated explicitly — `IpAddr::is_global` is still
/// unstable, so this is the allowlist-by-exclusion every SSRF guard writes
/// by hand.
pub fn ip_is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_is_public(v4),
        IpAddr::V6(v6) => ipv6_is_public(v6),
    }
}

fn ipv4_is_public(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0                        // 0.0.0.0/8 "this network"
        || ip.is_private()          // 10/8, 172.16/12, 192.168/16
        || ip.is_loopback()         // 127/8
        || ip.is_link_local()       // 169.254/16 (cloud metadata endpoint)
        || ip.is_broadcast()        // 255.255.255.255
        || ip.is_documentation()    // 192.0.2/24, 198.51.100/24, 203.0.113/24
        || (a == 100 && (64..=127).contains(&b)) // 100.64.0.0/10 CGNAT
        || (a == 192 && b == 0)     // 192.0.0.0/24 IETF protocol assignments
        || (a == 192 && b == 88 && c == 99) // 192.88.99.0/24 6to4 relay anycast
        || (a == 198 && (b == 18 || b == 19)) // 198.18.0.0/15 benchmarking
        || a >= 240) // 240.0.0.0/4 reserved
}

/// The IPv4 address an IPv6 transition address carries, if any.
///
/// This is the gap that mattered. `to_ipv4_mapped` covers `::ffff:a.b.c.d`,
/// but three other well-known formats also *embed* a v4 address, and each one
/// used to sail straight past every rule above:
///
/// - **NAT64** `64:ff9b::/96` — on any network with a NAT64 gateway (most
///   IPv6-only mobile networks, which is exactly where Kitty runs on Android),
///   `http://[64:ff9b::7f00:1]/` is a live route to `127.0.0.1`.
/// - **6to4** `2002::/16` — the v4 address is segments 1–2.
/// - **Teredo** `2001::/32` — the client v4 address is segments 6–7, stored
///   obfuscated (bitwise NOT).
///
/// Returning the embedded address here lets `ipv6_is_public` ask the v4
/// question about it, which is the same move `to_ipv4_mapped` already earns.
fn embedded_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return Some(mapped);
    }
    let seg = ip.segments();
    // NAT64 well-known prefix: 64:ff9b::/96 (and 64:ff9b:1::/48, the local-use
    // prefix from RFC 8215, whose v4 also sits in the last two segments).
    if seg[0] == 0x0064 && seg[1] == 0xff9b {
        return Some(v4_from(seg[6], seg[7]));
    }
    // 6to4: 2002:V4ADDR::/48
    if seg[0] == 0x2002 {
        return Some(v4_from(seg[1], seg[2]));
    }
    // Teredo: 2001:0::/32, client address in the last two segments, inverted.
    if seg[0] == 0x2001 && seg[1] == 0x0000 {
        return Some(v4_from(!seg[6], !seg[7]));
    }
    None
}

fn v4_from(hi: u16, lo: u16) -> Ipv4Addr {
    Ipv4Addr::new(
        (hi >> 8) as u8,
        (hi & 0xff) as u8,
        (lo >> 8) as u8,
        (lo & 0xff) as u8,
    )
}

fn ipv6_is_public(ip: Ipv6Addr) -> bool {
    // Any address that embeds an IPv4 address must answer the IPv4 question,
    // or every v4 rule above is trivially bypassed by re-encoding.
    if let Some(embedded) = embedded_ipv4(ip) {
        return ipv4_is_public(embedded);
    }
    let seg = ip.segments();
    !(ip.is_unspecified()                    // ::
        || ip.is_loopback()                  // ::1
        || (seg[0] & 0xffc0) == 0xfe80       // fe80::/10 link-local
        || (seg[0] & 0xfe00) == 0xfc00       // fc00::/7 unique-local
        || (seg[0] & 0xff00) == 0xff00       // ff00::/8 multicast
        || (seg[0] == 0x0100 && seg[1] == 0 && seg[2] == 0 && seg[3] == 0) // 100::/64 discard-only
        || (seg[0] == 0x2001 && seg[1] == 0x0db8)) // 2001:db8::/32 documentation
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(url: &str) -> bool {
        check_url(&url::Url::parse(url).unwrap()).is_err()
    }

    #[test]
    fn rejects_non_http_schemes() {
        assert!(blocked("file:///etc/passwd"));
        assert!(blocked("ftp://example.com/x"));
        assert!(blocked("gopher://example.com/"));
        assert!(blocked("data:text/html,<p>hi</p>"));
    }

    #[test]
    fn rejects_loopback_and_unspecified_literals() {
        assert!(blocked("http://127.0.0.1:8080/api"));
        assert!(blocked("http://127.1/"));
        assert!(blocked("http://[::1]/"));
        assert!(blocked("http://0.0.0.0/"));
        assert!(blocked("http://[::]/"));
        // IPv4-mapped IPv6 must not bypass the v4 rules.
        assert!(blocked("http://[::ffff:127.0.0.1]/"));
        assert!(blocked("http://[::ffff:10.0.0.1]/"));
    }

    #[test]
    fn rejects_private_and_link_local_literals() {
        assert!(blocked("http://10.0.0.1/"));
        assert!(blocked("http://172.16.0.1/"));
        assert!(blocked("http://172.31.255.254/"));
        assert!(blocked("http://192.168.1.1/"));
        assert!(blocked("http://169.254.169.254/latest/meta-data"));
        assert!(blocked("http://100.64.0.1/"));
        assert!(blocked("http://[fe80::1]/"));
        assert!(blocked("http://[fc00::1]/"));
        assert!(blocked("http://[fd00::1]/"));
    }

    #[test]
    fn rejects_reserved_literals() {
        assert!(blocked("http://192.0.2.1/"));
        assert!(blocked("http://198.18.0.1/"));
        assert!(blocked("http://240.0.0.1/"));
        assert!(blocked("http://255.255.255.255/"));
        assert!(blocked("http://[2001:db8::1]/"));
        assert!(blocked("http://[ff02::1]/"));
    }

    /// The gap `to_ipv4_mapped` alone left open: three other IPv6 formats
    /// embed a v4 address, and re-encoding an internal target in one of them
    /// used to walk past every v4 rule. NAT64 is the one that matters most
    /// here — it is a live route to the embedded address on the IPv6-only
    /// mobile networks Kitty runs on.
    #[test]
    fn rejects_internal_addresses_re_encoded_as_ipv6_transition_formats() {
        // NAT64 (64:ff9b::/96) wrapping 127.0.0.1, 169.254.169.254, 10.0.0.1.
        assert!(blocked("http://[64:ff9b::7f00:1]/"));
        assert!(blocked("http://[64:ff9b::a9fe:a9fe]/latest/meta-data"));
        assert!(blocked("http://[64:ff9b::a00:1]/"));
        // 6to4 (2002::/16) wrapping 127.0.0.1 and 192.168.1.1.
        assert!(blocked("http://[2002:7f00:1::]/"));
        assert!(blocked("http://[2002:c0a8:101::]/"));
        // Teredo (2001:0::/32) — the client v4 is stored inverted, so
        // !0x8071 !0xfffe == 127.142.0.1... use the exact inverse of
        // 127.0.0.1 (0x7f00, 0x0001) => 0x80ff, 0xfffe.
        assert!(blocked("http://[2001:0:0:0:0:0:80ff:fffe]/"));
        // 100::/64 discard-only.
        assert!(blocked("http://[100::1]/"));
        // 6to4 relay anycast.
        assert!(blocked("http://192.88.99.1/"));
    }

    /// The transition prefixes must not become blanket blocks: a 6to4 or
    /// NAT64 address wrapping a genuinely public v4 is fine, and rejecting
    /// those would break real IPv6-only clients.
    #[test]
    fn transition_addresses_wrapping_public_v4_are_still_allowed() {
        // 8.8.8.8 == 0x0808:0808
        assert!(!blocked("http://[64:ff9b::808:808]/"));
        assert!(!blocked("http://[2002:808:808::]/"));
    }

    #[test]
    fn allows_public_ip_literals() {
        assert!(!blocked("http://8.8.8.8/"));
        assert!(!blocked("https://1.1.1.1/path?q=1"));
        assert!(!blocked("http://[2001:4860:4860::8888]/"));
    }

    #[test]
    fn userinfo_and_ports_do_not_confuse_host_extraction() {
        // The host is what follows the last `@`, never the userinfo decoy.
        assert!(blocked("http://example.com@127.0.0.1/"));
        assert!(blocked("http://user@10.1.2.3:9000/"));
    }
}
