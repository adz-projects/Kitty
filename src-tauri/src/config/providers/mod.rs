//! Provider profiles. Profile *metadata* lives in app config; secrets live
//! only in the Windows Credential Manager via `keyring` — never on disk in
//! plaintext (CLAUDE.md rule 4). Activating a profile registers it with the
//! BigTiny daemon over REST (see `bigtiny::providers::sync_active_provider`).

mod connection;
mod keyring;
mod network;

pub use connection::test_connection;
pub use keyring::{
    delete_secret, get_or_create_bigtiny_encryption_key, get_secret_async, get_secret_checked,
    has_secret, migrate_secrets, set_secret,
};
pub use network::{network_tier_for, NetworkTier};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};

use crate::state::AppState;

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
    /// User-declared trust (Round-2 item 18). Loopback is always trusted by tier;
    /// this makes a non-loopback provider trusted (globe) instead of untrusted (⚠).
    #[serde(default)]
    pub is_trusted: bool,
    /// Per-provider sampling params (Round-2 item 27). `None` = provider/model
    /// default (BigTiny omits the field from the completion request entirely
    /// rather than sending an explicit default — see
    /// `bigtiny::providers::sync_active_provider`).
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    /// llama.cpp/Ollama sampling extension, no equivalent on hosted
    /// OpenAI-compatible or Anthropic endpoints. BigTiny only ever sends it
    /// for a `provider_type` of `ollama`/`custom_openai`
    /// (`bigtiny_rust::provider::openai_compat`), so setting it on a hosted
    /// profile is a silent no-op rather than an error.
    #[serde(default)]
    pub top_k: Option<i32>,
    /// Same scoping as `top_k`.
    #[serde(default)]
    pub min_p: Option<f32>,
    /// Repetition control. `None` here does not mean "off" the way it does
    /// for `temperature`/`top_p` — BigTiny fills in a repetition-safe
    /// default for self-hosted providers when this is unset (see
    /// `bigtiny_rust::provider::sampling::defaults_for`), because
    /// llama-server's own default disables repetition control entirely.
    /// Set this explicitly only to override that default.
    #[serde(default)]
    pub presence_penalty: Option<f32>,
    #[serde(default)]
    pub frequency_penalty: Option<f32>,
    /// Hard cap on one reply's length. `None` gets BigTiny's own default for
    /// self-hosted providers (see `presence_penalty`'s doc comment) — set
    /// this to override, not to enable a cap that doesn't otherwise exist.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub context_length: Option<u32>,
    /// Strip the model's own prior reasoning out of what gets resent as context
    /// on later turns (chat-only mode only — see `chatStore.ts`'s `send()`).
    /// STOPGAP (client-side workaround): Goose has no native hook for this
    /// (confirmed: no ACP method, no env var, no config key; reasoning-in-history
    /// handling is hardcoded per-provider in goosed's own Rust source). Remove
    /// this field and the whole client-side session-swap path it drives once
    /// Goose implements https://github.com/block/goose/issues/7617 or an
    /// equivalent native mechanism, and thread it into `goosed_env()` instead,
    /// matching `temperature`/`top_p`/`context_length` below.
    #[serde(default)]
    pub strip_reasoning: bool,
    /// Custom system prompt for this provider (Round-6 Feature 2). `None` =
    /// use the built-in mode-appropriate default (see
    /// `src/lib/system_prompts.ts`). STOPGAP-adjacent, same rationale as
    /// `strip_reasoning` above: Goose's ACP `session/new` silently drops
    /// unknown params like `systemPrompt`/`instructions` (live-probed,
    /// `docs/acp-protocol.md`), and there is no `GOOSE_*` env var for this
    /// either, so the resolved prompt is prepended client-side to a session's
    /// first outgoing message (`chatStore.ts`'s `send()`) rather than passed
    /// through ACP. This field is never sent as an env var — no
    /// `goosed_env()` change needed. Revisit if Goose ever gains native
    /// system-prompt support, or once the `.goosehints` file convention is
    /// probe-confirmed as a cleaner alternative (see the plan doc's deferred
    /// Batch 9).
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Override for `session/prompt`'s idle-reset timeout window (default 300s —
    /// see `AcpClient::request_session_prompt`'s doc comment). `None` = use the
    /// default. Useful for a model/provider known to have long gaps between
    /// streamed updates (e.g. a slow Tailscale-hosted host) where the default
    /// is too eager, or conversely one where a long silence reliably means
    /// "stuck" and the user would rather find out sooner than wait 5 minutes.
    #[serde(default)]
    pub prompt_idle_timeout_secs: Option<u32>,
    /// The `-np`/`--parallel` slot count this provider's own llama-server(-
    /// compatible) endpoint was started with, when known. `None` (the
    /// default) means: never pin this provider's turns to a KV-cache slot —
    /// correct for Ollama and anything not deliberately running a
    /// multi-slot llama-server. Threaded through to BigTiny's
    /// `ProviderConfig::parallel_slots` (`bigtiny::providers::
    /// sync_active_provider`), which derives `id_slot` per turn from it. A
    /// value here that doesn't match the real server's `--parallel` doesn't
    /// error — it just silently thrashes the KV cache instead of pinning
    /// it, so this is deliberately a plain number the user sets to match
    /// their own server config, not something Kitty can discover on its
    /// own.
    #[serde(default)]
    pub parallel_slots: Option<u32>,
    #[serde(default)]
    pub created_at: String,
}

impl ProviderProfile {
    pub fn network_tier(&self) -> NetworkTier {
        network_tier_for(&self.base_url)
    }
}

/// Reachability for Personal/Remote providers is derived from real send
/// outcomes (Round-3 item 19, revised) rather than a speculative background
/// ping — this app makes no inference calls of its own, so a failed/succeeded
/// `session/prompt` is a strictly better signal than a periodic GET. Call this
/// from `send_prompt`'s completion handler with whether that send succeeded;
/// it's a no-op for a `Local`-tier active provider (which has nothing to be
/// unreachable in the Tailscale/cloud sense — the local stack loop covers it).
pub fn emit_health_from_send_result(app: &AppHandle, reachable: bool) {
    let active = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        cfg.active_provider_id
            .as_ref()
            .and_then(|id| cfg.providers.iter().find(|p| &p.id == id).cloned())
    };
    let Some(p) = active.filter(|p| !matches!(network_tier_for(&p.base_url), NetworkTier::Local))
    else {
        return;
    };
    let host = network::host_of(&p.base_url);
    let _ = app.emit(
        "provider://health",
        json!({ "reachable": reachable, "host": host, "name": p.name }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_shape_provider_migrates_with_defaults() {
        // A profile written before Round-2 (no is_trusted / temperature / etc.)
        // must still deserialize, defaulting the new fields. Also carries the
        // since-removed `tools_enabled` (Round-7: dropped in favor of the
        // per-session chat/agentic toggle) to confirm a stale field is silently
        // ignored rather than erroring.
        let json = r#"{
            "id": "p1", "name": "Box", "provider_type": "ollama",
            "base_url": "http://localhost:11434", "models": ["llama3.2:3b"],
            "tools_enabled": true, "created_at": "2026-01-01T00:00:00Z"
        }"#;
        let p: ProviderProfile = serde_json::from_str(json).unwrap();
        assert!(!p.is_trusted);
        assert_eq!(p.temperature, None);
        assert_eq!(p.top_p, None);
        assert_eq!(p.top_k, None);
        assert_eq!(p.min_p, None);
        assert_eq!(p.presence_penalty, None);
        assert_eq!(p.frequency_penalty, None);
        assert_eq!(p.max_tokens, None);
        assert_eq!(p.context_length, None);
        assert_eq!(p.models, vec!["llama3.2:3b"]);
        assert!(!p.strip_reasoning);
        assert_eq!(p.system_prompt, None);
        assert_eq!(p.prompt_idle_timeout_secs, None);
        assert_eq!(p.parallel_slots, None);
    }

    /// Phase 2b acceptance: an `ollama` profile saved while Kitty still
    /// managed an Ollama process must keep working afterwards, as a *remote*
    /// endpoint pointed at a server the user runs.
    ///
    /// The risk isn't deserialization — `provider_type` is an untyped
    /// `String`, so it was never going to fail to load. It's that the row
    /// becomes decorative: still listed, no longer routable. These assertions
    /// pin the two things that keep it live, both of which are easy to delete
    /// by accident while removing "the Ollama code": the profile still
    /// reaches BigTiny as an `openai_compat` provider, and its granular type
    /// still rides along as `provider_dialect`, which is the *only* channel
    /// telling the daemon to apply the self-hosted sampling floor and put
    /// `top_k`/`min_p` on the wire.
    #[test]
    fn a_legacy_ollama_profile_still_routes_after_managed_ollama_was_removed() {
        let json = r#"{
            "id": "p1", "name": "My Ollama", "provider_type": "ollama",
            "base_url": "http://192.168.1.50:11434", "models": ["qwen3.5:4b"],
            "created_at": "2026-01-01T00:00:00Z"
        }"#;
        let p: ProviderProfile = serde_json::from_str(json).unwrap();
        assert_eq!(p.provider_type, "ollama");

        let (wire_type, base) = crate::bigtiny::providers::bigtiny_provider_target(&p);
        assert_eq!(wire_type, "openai_compat");
        assert_eq!(base, "http://192.168.1.50:11434");

        // Not "local": a server the user runs needs nothing of ours on disk,
        // so it must not make the app report a missing local model.
        assert_ne!(p.provider_type, "local");
    }
}
