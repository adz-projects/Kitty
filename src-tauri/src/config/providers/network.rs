//! Network-privacy tier classification for a provider's `base_url`.

use serde::{Deserialize, Serialize};

/// Network-privacy tier, computed from the profile's `base_url` host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkTier {
    /// localhost / loopback.
    Local,
    /// Tailscale (CGNAT 100.64.0.0/10 or `*.ts.net`) — private but can go offline.
    Personal,
    /// Anything else, incl. plain LAN — treat as third-party.
    Remote,
}

/// Extract the host from a base URL and classify its network tier.
pub fn network_tier_for(base_url: &str) -> NetworkTier {
    let host = host_of(base_url);
    let h = host.to_ascii_lowercase();
    if h.is_empty() || h == "localhost" || h == "127.0.0.1" || h == "::1" || h == "[::1]" {
        return NetworkTier::Local;
    }
    if h.ends_with(".ts.net") || in_cgnat(&h) {
        return NetworkTier::Personal;
    }
    NetworkTier::Remote
}

pub fn host_of(base_url: &str) -> String {
    let no_scheme = base_url.split("://").last().unwrap_or(base_url);
    let host_port = no_scheme.split('/').next().unwrap_or("");
    // Strip an optional userinfo@ and a :port (ignore IPv6 brackets for simplicity).
    let after_at = host_port.rsplit('@').next().unwrap_or(host_port);
    if after_at.starts_with('[') {
        // Bracketed IPv6: the host ends at the closing `]` — anything after
        // it is `:port` and must not ride along, or `http://[::1]:11434`
        // yields `"[::1]:11434"`, which fails the loopback compare in
        // `network_tier_for` and misclassifies a loopback daemon as Remote.
        if let Some(end) = after_at.find(']') {
            return after_at[..=end].to_string();
        }
        return after_at.to_string();
    }
    after_at.split(':').next().unwrap_or(after_at).to_string()
}

/// Tailscale CGNAT range 100.64.0.0/10 (100.64.0.0 – 100.127.255.255).
fn in_cgnat(host: &str) -> bool {
    let octets: Vec<u8> = host.split('.').filter_map(|o| o.parse().ok()).collect();
    octets.len() == 4 && octets[0] == 100 && (64..=127).contains(&octets[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_classify_correctly() {
        assert_eq!(
            network_tier_for("http://localhost:11434"),
            NetworkTier::Local
        );
        assert_eq!(
            network_tier_for("http://127.0.0.1:1234"),
            NetworkTier::Local
        );
        assert_eq!(
            network_tier_for("http://100.101.5.6:11434"),
            NetworkTier::Personal
        );
        assert_eq!(
            network_tier_for("https://box.tail1234.ts.net"),
            NetworkTier::Personal
        );
        assert_eq!(
            network_tier_for("https://openrouter.ai/api/v1"),
            NetworkTier::Remote
        );
        // Plain LAN is treated as remote, not personal.
        assert_eq!(
            network_tier_for("http://192.168.1.50:11434"),
            NetworkTier::Remote
        );
    }

    /// Regression (815bugs #10): a bracketed IPv6 host used to keep its
    /// `:port`, so `http://[::1]:11434` — exactly how a local IPv6 Ollama /
    /// llama-server URL is written — was misclassified Remote.
    #[test]
    fn bracketed_ipv6_host_drops_the_port() {
        assert_eq!(host_of("http://[::1]:11434"), "[::1]");
        assert_eq!(host_of("http://[2001:db8::10]:8080/v1"), "[2001:db8::10]");
        assert_eq!(
            network_tier_for("http://[::1]:11434"),
            NetworkTier::Local
        );
        assert_eq!(
            network_tier_for("http://[2001:db8::10]:8080"),
            NetworkTier::Remote
        );
    }
}
