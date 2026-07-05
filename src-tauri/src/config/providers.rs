//! Provider profiles (Phase 5). Profile *metadata* lives in app config; secrets
//! live only in the Windows Credential Manager via `keyring` — never on disk in
//! plaintext (CLAUDE.md rule 4). Activating a profile routes goosed to that
//! provider by injecting Goose's env vars when we (re)spawn `goose serve`.

use serde::{Deserialize, Serialize};

use crate::config::Config;

const KEYRING_SERVICE: &str = "goose-overlay";

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

/// A named provider profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProfile {
    pub id: String,
    pub name: String,
    /// `ollama` | `openrouter` | `anthropic` | `openai` | `custom_openai`.
    pub provider_type: String,
    pub base_url: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default = "default_true")]
    pub tools_enabled: bool,
    /// User-declared trust (Round-2 item 18). Loopback is always trusted by tier;
    /// this makes a non-loopback provider trusted (globe) instead of untrusted (⚠).
    #[serde(default)]
    pub is_trusted: bool,
    /// Per-provider sampling params (Round-2 item 27). `None` = use Goose default.
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub context_length: Option<u32>,
    #[serde(default)]
    pub created_at: String,
}

fn default_true() -> bool {
    true
}

impl ProviderProfile {
    pub fn network_tier(&self) -> NetworkTier {
        network_tier_for(&self.base_url)
    }
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

fn host_of(base_url: &str) -> String {
    let no_scheme = base_url.split("://").last().unwrap_or(base_url);
    let host_port = no_scheme.split('/').next().unwrap_or("");
    // Strip an optional userinfo@ and a :port (ignore IPv6 brackets for simplicity).
    let after_at = host_port.rsplit('@').next().unwrap_or(host_port);
    if after_at.starts_with('[') {
        return after_at.to_string();
    }
    after_at.split(':').next().unwrap_or(after_at).to_string()
}

/// Tailscale CGNAT range 100.64.0.0/10 (100.64.0.0 – 100.127.255.255).
fn in_cgnat(host: &str) -> bool {
    let octets: Vec<u8> = host.split('.').filter_map(|o| o.parse().ok()).collect();
    octets.len() == 4 && octets[0] == 100 && (64..=127).contains(&octets[1])
}

// --- Secrets (keyring) ---

fn entry(id: &str) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, id)
}

pub fn set_secret(id: &str, secret: &str) -> Result<(), String> {
    entry(id)
        .and_then(|e| e.set_password(secret))
        .map_err(|e| format!("could not store secret: {e}"))
}

pub fn get_secret(id: &str) -> Option<String> {
    entry(id).ok().and_then(|e| e.get_password().ok())
}

pub fn delete_secret(id: &str) {
    if let Ok(e) = entry(id) {
        let _ = e.delete_credential();
    }
}

pub fn has_secret(id: &str) -> bool {
    get_secret(id).is_some()
}

// --- goosed provider env ---

/// Map our provider_type to Goose's `GOOSE_PROVIDER` value.
fn goose_provider_name(provider_type: &str) -> &str {
    match provider_type {
        "custom_openai" => "openai",
        other => other,
    }
}

/// Build the environment for `goose serve` from the active provider profile +
/// model params. Empty when no profile is active (goosed uses its own config).
pub fn goosed_env(config: &Config) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();

    if let Some(active) = config
        .active_provider_id
        .as_ref()
        .and_then(|id| config.providers.iter().find(|p| &p.id == id))
    {
        env.push((
            "GOOSE_PROVIDER".into(),
            goose_provider_name(&active.provider_type).into(),
        ));
        if let Some(model) = active.models.first() {
            env.push(("GOOSE_MODEL".into(), model.clone()));
        }
        let secret = get_secret(&active.id);
        match active.provider_type.as_str() {
            "ollama" => env.push(("OLLAMA_HOST".into(), active.base_url.clone())),
            "openrouter" => {
                if let Some(s) = secret {
                    env.push(("OPENROUTER_API_KEY".into(), s));
                }
            }
            "anthropic" => {
                if let Some(s) = secret {
                    env.push(("ANTHROPIC_API_KEY".into(), s));
                }
            }
            "openai" | "custom_openai" => {
                if let Some(s) = secret {
                    env.push(("OPENAI_API_KEY".into(), s));
                }
                env.push(("OPENAI_BASE_URL".into(), active.base_url.clone()));
                env.push(("OPENAI_HOST".into(), active.base_url.clone()));
            }
            _ => {}
        }

        // Per-provider sampling params (Round-2 item 27; None -> Goose default).
        if let Some(t) = active.temperature {
            env.push(("GOOSE_TEMPERATURE".into(), t.to_string()));
        }
        if let Some(c) = active.context_length {
            env.push(("GOOSE_CONTEXT_LIMIT".into(), c.to_string()));
        }
        if let Some(p) = active.top_p {
            env.push(("GOOSE_TOP_P".into(), p.to_string()));
        }
    }

    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_classify_correctly() {
        assert_eq!(network_tier_for("http://localhost:11434"), NetworkTier::Local);
        assert_eq!(network_tier_for("http://127.0.0.1:1234"), NetworkTier::Local);
        assert_eq!(network_tier_for("http://100.101.5.6:11434"), NetworkTier::Personal);
        assert_eq!(network_tier_for("https://box.tail1234.ts.net"), NetworkTier::Personal);
        assert_eq!(network_tier_for("https://openrouter.ai/api/v1"), NetworkTier::Remote);
        // Plain LAN is treated as remote, not personal.
        assert_eq!(network_tier_for("http://192.168.1.50:11434"), NetworkTier::Remote);
    }

    #[test]
    fn old_shape_provider_migrates_with_defaults() {
        // A profile written before Round-2 (no is_trusted / temperature / etc.)
        // must still deserialize, defaulting the new fields.
        let json = r#"{
            "id": "p1", "name": "Box", "provider_type": "ollama",
            "base_url": "http://localhost:11434", "models": ["llama3.2:3b"],
            "tools_enabled": true, "created_at": "2026-01-01T00:00:00Z"
        }"#;
        let p: ProviderProfile = serde_json::from_str(json).unwrap();
        assert!(!p.is_trusted);
        assert_eq!(p.temperature, None);
        assert_eq!(p.top_p, None);
        assert_eq!(p.context_length, None);
        assert_eq!(p.models, vec!["llama3.2:3b"]);
    }
}
