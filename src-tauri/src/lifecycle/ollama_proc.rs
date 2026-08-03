//! Ollama process detection + (conditional) spawning and health probes.
//! We never call generate/chat here — inference goes through BigTiny. We only
//! manage the process and read `/api/version` and `/api/tags`.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::config::Config;
use crate::state::ManagedProcess;
use crate::util::{capture_output, hidden_command};

/// Shared timeout for the local health/model probes below — these hit
/// loopback/LAN Ollama, so a generous fixed value is simpler than threading a
/// caller-specific duration through every probe, and no caller actually
/// needed a different SLA (the old per-call values were incidental).
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// `GET /api/version` — treat any successful HTTP response as "up".
pub async fn probe_version(client: &reqwest::Client, base_url: &str) -> bool {
    let url = format!("{}/api/version", base_url.trim_end_matches('/'));
    client.get(url).timeout(PROBE_TIMEOUT).send().await.is_ok()
}

/// `GET /api/tags` — true if at least one model is installed.
pub async fn has_any_model(client: &reqwest::Client, base_url: &str) -> bool {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    match client.get(url).timeout(PROBE_TIMEOUT).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(json) => json
                .get("models")
                .and_then(|m| m.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false),
            Err(_) => false,
        },
        Err(_) => false,
    }
}

/// `GET /api/tags` — true if the specific `tag` (e.g. `qwen3-embedding:0.6b`)
/// is installed. Unlike `has_any_model`, this checks for one exact model —
/// used to gate the adaptive-pathway embedding-model auto-pull, which cares
/// about a specific tag being present, not just "some model or other."
pub async fn has_model_tag(client: &reqwest::Client, base_url: &str, tag: &str) -> bool {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    match client.get(url).timeout(PROBE_TIMEOUT).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(json) => tags_response_has_tag(&json, tag),
            Err(_) => false,
        },
        Err(_) => false,
    }
}

/// Pure matching logic behind `has_model_tag`, split out so it's unit
/// testable without standing up a live (or mocked) Ollama server.
fn tags_response_has_tag(json: &serde_json::Value, tag: &str) -> bool {
    json.get("models")
        .and_then(|m| m.as_array())
        .map(|a| {
            a.iter()
                .any(|m| m.get("name").and_then(|n| n.as_str()) == Some(tag))
        })
        .unwrap_or(false)
}

/// True when the current setup actually needs a locally-running Ollama: no
/// provider is active yet (fresh/local-default state), or the active
/// provider's `provider_type` is `"ollama"`. `false` for any remote/API-key
/// provider — used to skip `start_stack`'s Ollama `ensure_running` step and
/// `compute_status`'s `OllamaDown`/`NoModel` checks so a healthy remote-only
/// setup doesn't misreport as broken (wizard redesign: the local-vs-API-key
/// fork means Ollama is no longer a hard requirement for every install).
pub fn requires_local_ollama(config: &Config) -> bool {
    match config
        .active_provider_id
        .as_ref()
        .and_then(|id| config.providers.iter().find(|p| &p.id == id))
    {
        Some(p) => p.provider_type == "ollama",
        None => true,
    }
}

/// Locate the `ollama` binary: `OLLAMA_BIN` override, the default per-user
/// install dir, then bare `ollama` on PATH.
pub fn locate_ollama() -> PathBuf {
    if let Ok(p) = std::env::var("OLLAMA_BIN") {
        let path = PathBuf::from(p);
        if path.exists() {
            return path;
        }
    }
    if let Some(local) = dirs::data_local_dir() {
        let candidate = local.join("Programs").join("Ollama").join("ollama.exe");
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from("ollama")
}

/// Ensure Ollama is reachable. If already up, we do not own it. If down and a
/// binary exists, spawn `ollama serve` and mark it owned.
pub async fn ensure_running(base_url: &str) -> Result<ManagedProcess, String> {
    let client = crate::util::http_client();

    if probe_version(&client, base_url).await {
        // Already running — not ours; never kill it.
        return Ok(ManagedProcess {
            child: None,
            owned: false,
        });
    }

    let bin = locate_ollama();
    let mut child = hidden_command(&bin)
        .arg("serve")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn ollama serve ({}): {e}", bin.display()))?;
    capture_output(&mut child, "ollama");

    // Give it a moment to bind before the first health probe.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    Ok(ManagedProcess {
        child: Some(child),
        owned: true,
    })
}

/// Kept for symmetry / future use: whether a bare `ollama` resolves on PATH.
#[allow(dead_code)]
pub fn ollama_on_path() -> bool {
    Command::new("ollama").arg("--version").output().is_ok()
}

#[cfg(test)]
mod tests {
    use super::{requires_local_ollama, tags_response_has_tag};
    use crate::config::providers::ProviderProfile;
    use crate::config::Config;
    use serde_json::json;

    #[test]
    fn requires_local_ollama_reflects_active_provider() {
        let mut cfg = Config::default();
        // No active provider yet: default to requiring Ollama (fresh install).
        assert!(requires_local_ollama(&cfg));

        cfg.providers.push(ProviderProfile {
            id: "p1".into(),
            name: "Claude".into(),
            provider_type: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            models: vec!["claude-sonnet-5".into()],
            is_trusted: true,
            temperature: None,
            top_p: None,
            top_k: None,
            min_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            max_tokens: None,
            context_length: None,
            strip_reasoning: false,
            system_prompt: None,
            prompt_idle_timeout_secs: None,
            parallel_slots: None,
            created_at: String::new(),
        });
        cfg.active_provider_id = Some("p1".into());
        assert!(!requires_local_ollama(&cfg));

        cfg.providers.push(ProviderProfile {
            id: "p2".into(),
            name: "Local".into(),
            provider_type: "ollama".into(),
            base_url: "http://localhost:11434".into(),
            models: vec!["llama3.2:3b".into()],
            is_trusted: false,
            temperature: None,
            top_p: None,
            top_k: None,
            min_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            max_tokens: None,
            context_length: None,
            strip_reasoning: false,
            system_prompt: None,
            prompt_idle_timeout_secs: None,
            parallel_slots: None,
            created_at: String::new(),
        });
        cfg.active_provider_id = Some("p2".into());
        assert!(requires_local_ollama(&cfg));
    }

    #[test]
    fn tags_response_has_tag_matches_exact_name() {
        let body = json!({
            "models": [
                {"name": "llama3.2:3b"},
                {"name": "qwen3-embedding:0.6b"},
            ]
        });
        assert!(tags_response_has_tag(&body, "qwen3-embedding:0.6b"));
    }

    #[test]
    fn tags_response_has_tag_rejects_different_size_tag() {
        // 0.6b vs 4b/8b are different models/latent spaces (never mix) —
        // confirms the match is on the exact tag string, not a name prefix.
        let body = json!({ "models": [{"name": "qwen3-embedding:4b"}] });
        assert!(!tags_response_has_tag(&body, "qwen3-embedding:0.6b"));
    }

    #[test]
    fn tags_response_has_tag_false_when_absent() {
        let body = json!({ "models": [{"name": "llama3.2:3b"}] });
        assert!(!tags_response_has_tag(&body, "qwen3-embedding:0.6b"));
    }

    #[test]
    fn tags_response_has_tag_false_on_empty_models() {
        let body = json!({ "models": [] });
        assert!(!tags_response_has_tag(&body, "qwen3-embedding:0.6b"));
    }

    #[test]
    fn tags_response_has_tag_false_on_malformed_response() {
        let body = json!({ "unexpected": "shape" });
        assert!(!tags_response_has_tag(&body, "qwen3-embedding:0.6b"));
    }
}
