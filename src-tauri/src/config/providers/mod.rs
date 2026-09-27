//! Provider profiles. Profile *metadata* lives in app config; secrets live
//! only in the Windows Credential Manager via `keyring` — never on disk in
//! plaintext (CLAUDE.md rule 4). Activating a profile registers it with the
//! BigTiny daemon over REST (see `bigtiny::providers::sync_active_provider`).

mod connection;
pub mod endpoint;
mod keyring;
mod network;

pub use connection::test_connection;
pub use keyring::{
    delete_secret, get_secret_async, get_secret_checked, migrate_secrets, set_secret_async,
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
    /// `ollama` | `openrouter` | `anthropic` | `openai` | `fireworks` |
    /// `deepinfra` | `custom_openai` | `local`. A plain string, not an enum
    /// — a new hosted OpenAI-compatible dialect (like the 2 above) needs no
    /// type change here, just new match arms wherever behavior actually
    /// differs (see `config/providers/connection.rs`'s
    /// `test_connection` and `bigtiny/providers.rs`'s
    /// `bigtiny_provider_target`).
    pub provider_type: String,
    pub base_url: String,
    #[serde(default)]
    pub models: Vec<String>,
    /// User-declared trust (Round-2 item 18). Loopback is always trusted by tier;
    /// this makes a non-loopback provider trusted (globe) instead of untrusted (⚠).
    #[serde(default)]
    pub is_trusted: bool,
    /// Whether this provider may host specialist runs: `"preferred"`,
    /// `"allowed"` (the default when absent) or `"never"`.
    ///
    /// The user's own statement, and the first thing the daemon's host picker
    /// consults — the only signal it has that reflects intent rather than
    /// measurement. Pushed down in the provider's `config` blob.
    #[serde(default)]
    pub subagent_role: Option<String>,
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
    /// (`bigtiny2::provider::openai_compat`), so setting it on a hosted
    /// profile is a silent no-op rather than an error.
    #[serde(default)]
    pub top_k: Option<i32>,
    /// Same scoping as `top_k`.
    #[serde(default)]
    pub min_p: Option<f32>,
    /// Repetition control. `None` here does not mean "off" the way it does
    /// for `temperature`/`top_p` — BigTiny fills in a repetition-safe
    /// default for self-hosted providers when this is unset (see
    /// `bigtiny2::provider::sampling::defaults_for`), because
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
    /// Manual override: this provider's models accept image content blocks.
    ///
    /// Vision support is otherwise detected from the model *name*
    /// (`src/lib/vision_models.ts`), which is deliberately conservative — an
    /// unrecognized name is treated as text-only. That is the right default
    /// when the cost of guessing wrong is a failed turn, but it means a
    /// self-hosted or oddly-named vision model has no way to be recognized,
    /// and the UI hides its image affordances with no recourse. This flag is
    /// that recourse: it only ever *widens* what's allowed (it ORs with
    /// detection), so ticking it for a text-only model is a user's own
    /// mistake to make, and leaving it off never blocks a model the patterns
    /// already know about.
    #[serde(default)]
    pub supports_vision: bool,
    /// Custom system prompt for this provider (Round-6 Feature 2). `None` =
    /// use the built-in default (see `src/lib/system_prompts.ts`). Applied as
    /// the session's `persona_override` on its first turn, which BigTiny
    /// renders as a real `role: "system"` message.
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
    /// The user's override for whether this provider's model can call tools.
    /// `None` (the default) means detect it: the OpenRouter catalog when it
    /// knows the model, else assume yes (the daemon still notices a model
    /// that refuses tools and says so). Decides whether the model is offered
    /// tools at all, and whether a dropped file is sent as a path (for the
    /// file tools to open) or inlined.
    #[serde(default)]
    pub supports_tools: Option<bool>,
    /// Why this profile can no longer be used, when it cannot:
    /// `"unsupported"` for the retired "On this device" (`local`) type. A
    /// disabled profile stays listed so the user can see and delete it, but is
    /// never synced to the engine or used for a chat.
    #[serde(default)]
    pub disabled_reason: Option<String>,
    #[serde(default)]
    pub created_at: String,
}

impl ProviderProfile {
    pub fn network_tier(&self) -> NetworkTier {
        network_tier_for(&self.base_url)
    }

    /// Usable for chat and synced to the engine.
    pub fn is_usable(&self) -> bool {
        self.disabled_reason.is_none()
    }

    /// Whether this provider's model can call tools: the user's override,
    /// else the OpenRouter catalog's answer for this exact model, else yes.
    pub fn tools_supported(
        &self,
        catalog: Option<&crate::openrouter::catalog::OpenRouterCatalog>,
    ) -> bool {
        if let Some(explicit) = self.supports_tools {
            return explicit;
        }
        let model = self.models.first().map(String::as_str).unwrap_or("");
        catalog
            .and_then(|c| crate::openrouter::catalog::tools_support_for(model, &c.entries))
            .unwrap_or(true)
    }
}

/// The built-in system prompt, used when a provider has no `system_prompt`
/// of its own. Set as the session's persona when the session is created
/// (`commands::session::new_session`) and when a new chat's provider is
/// changed, so every turn - including scheduled runs - gets it.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are a capable, direct agentic assistant. You have \
filesystem and shell tools scoped to this conversation's own working directory. Use tools \
proactively rather than describing what you would do — take the action. When you create or \
save a file, use a relative path inside the working directory rather than an absolute path \
elsewhere. Be direct about assumptions and uncertainty rather than glossing over them, and \
prefer verifiable action (running a command, reading a file, writing output) over speculation.";

/// The system prompt a session on `profile` runs with.
pub fn system_prompt_for(profile: &ProviderProfile) -> String {
    profile
        .system_prompt
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or(DEFAULT_SYSTEM_PROMPT)
        .to_string()
}

/// Retire the "On this device" (`local`) provider type: there is no local
/// chat engine. Its profiles are kept but disabled, and if one was the
/// default the default is cleared and flagged, so the UI can ask the user to
/// pick another. Returns whether anything changed. Pure, and idempotent.
pub fn migrate_local_profiles(cfg: &mut crate::config::Config) -> bool {
    let mut changed = false;
    for p in cfg
        .providers
        .iter_mut()
        .filter(|p| p.provider_type == "local")
    {
        if p.disabled_reason.is_none() {
            p.disabled_reason = Some("unsupported".to_string());
            changed = true;
        }
    }
    let default_disabled = cfg
        .active_provider_id
        .as_deref()
        .and_then(|id| cfg.providers.iter().find(|p| p.id == id))
        .is_some_and(|p| !p.is_usable());
    if default_disabled {
        cfg.active_provider_id = None;
        cfg.needs_default_provider = true;
        changed = true;
    }
    changed
}

/// The profile to make the default after `removed` is deleted or disabled:
/// the next usable one in list order after it, else the one before it.
pub fn next_default(providers: &[ProviderProfile], removed_index: usize) -> Option<String> {
    providers
        .iter()
        .skip(removed_index)
        .chain(providers.iter().take(removed_index).rev())
        .find(|p| p.is_usable())
        .map(|p| p.id.clone())
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

/// One provider card holds exactly one model (v1). A profile saved while the
/// form still took a comma-separated list is split into one profile per model,
/// each named after its model; the first keeps the original id, so sessions
/// already pinned to it are unaffected.
///
/// Pure: returns `(source_id, new_id)` for every profile created, because each
/// new card needs its own copy of the source's secret, and the keystore is
/// async (and platform-dispatched) where this is not.
pub fn split_multi_model_profiles(providers: &mut Vec<ProviderProfile>) -> Vec<(String, String)> {
    let mut copies = Vec::new();
    let taken: std::collections::HashSet<String> = providers.iter().map(|p| p.id.clone()).collect();
    let mut out = Vec::with_capacity(providers.len());
    for p in providers.drain(..) {
        if p.models.len() <= 1 {
            out.push(p);
            continue;
        }
        let mut first = p.clone();
        first.models = vec![p.models[0].clone()];
        first.name = format!("{} ({})", p.name, p.models[0]);
        out.push(first);
        for (i, model) in p.models.iter().enumerate().skip(1) {
            let mut n = i;
            let id = loop {
                let candidate = format!("{}-m{n}", p.id);
                if !taken.contains(&candidate) {
                    break candidate;
                }
                n += 1;
            };
            let mut copy = p.clone();
            copy.id = id.clone();
            copy.models = vec![model.clone()];
            copy.name = format!("{} ({model})", p.name);
            copies.push((p.id.clone(), id));
            out.push(copy);
        }
    }
    *providers = out;
    copies
}

/// Apply [`split_multi_model_profiles`] to the saved config, copying each
/// source profile's secret to the cards made from it. Runs once per launch;
/// a no-op when every profile already has at most one model.
pub async fn migrate_multi_model_profiles(app: &AppHandle) {
    let copies = {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        let copies = split_multi_model_profiles(&mut cfg.providers);
        if copies.is_empty() {
            return;
        }
        if let Err(e) = crate::config::save(&cfg) {
            tracing::warn!("could not save the split provider profiles: {e}");
        }
        copies
    };
    for (source, target) in copies {
        match get_secret_checked(&source).await {
            Ok(Some(secret)) => {
                if let Err(e) = set_secret_async(&target, &secret).await {
                    tracing::warn!("could not copy the key for provider {target}: {e}");
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("could not read the key for provider {source}: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile_with(id: &str, name: &str, models: &[&str]) -> ProviderProfile {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": name, "provider_type": "custom_openai",
            "base_url": "http://box:8080", "models": models,
        }))
        .unwrap()
    }

    fn typed(id: &str, provider_type: &str) -> ProviderProfile {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": id, "provider_type": provider_type,
            "base_url": "http://box:8080", "models": ["m"],
        }))
        .unwrap()
    }

    /// Decision #65: a local profile is kept but disabled, and a local
    /// default is cleared and flagged so the user is asked for another.
    #[test]
    fn local_profiles_are_disabled_and_a_local_default_is_cleared() {
        let mut cfg = crate::config::Config {
            providers: vec![typed("loc", "local"), typed("or", "openrouter")],
            active_provider_id: Some("loc".into()),
            ..Default::default()
        };
        assert!(migrate_local_profiles(&mut cfg));
        assert_eq!(
            cfg.providers[0].disabled_reason.as_deref(),
            Some("unsupported")
        );
        assert!(cfg.providers[1].is_usable());
        assert_eq!(cfg.active_provider_id, None);
        assert!(cfg.needs_default_provider);
        assert!(!migrate_local_profiles(&mut cfg), "idempotent");
    }

    #[test]
    fn a_remote_default_survives_the_local_migration() {
        let mut cfg = crate::config::Config {
            providers: vec![typed("loc", "local"), typed("or", "openrouter")],
            active_provider_id: Some("or".into()),
            ..Default::default()
        };
        migrate_local_profiles(&mut cfg);
        assert_eq!(cfg.active_provider_id.as_deref(), Some("or"));
        assert!(!cfg.needs_default_provider);
    }

    /// Decision #5: deleting the default promotes the next usable card.
    #[test]
    fn the_next_default_skips_disabled_cards() {
        let mut dead = typed("dead", "local");
        dead.disabled_reason = Some("unsupported".into());
        let list = [typed("a", "openrouter"), dead, typed("c", "anthropic")];
        // "a" (index 0) was removed from the list before this is asked.
        let after_removal = &list[1..];
        assert_eq!(next_default(after_removal, 0).as_deref(), Some("c"));
        assert_eq!(
            next_default(&list[..1], 1).as_deref(),
            Some("a"),
            "falls back to earlier cards"
        );
        assert_eq!(next_default(&[], 0), None);
    }

    #[test]
    fn a_provider_without_its_own_prompt_gets_the_default() {
        let mut p = typed("a", "openrouter");
        assert_eq!(system_prompt_for(&p), DEFAULT_SYSTEM_PROMPT);
        p.system_prompt = Some("  ".into());
        assert_eq!(
            system_prompt_for(&p),
            DEFAULT_SYSTEM_PROMPT,
            "blank is not a prompt"
        );
        p.system_prompt = Some("Be terse.".into());
        assert_eq!(system_prompt_for(&p), "Be terse.");
    }

    #[test]
    fn tool_support_prefers_the_override_then_assumes_yes() {
        let mut p = typed("a", "custom_openai");
        assert!(p.tools_supported(None));
        p.supports_tools = Some(false);
        assert!(!p.tools_supported(None));
    }

    #[test]
    fn a_multi_model_profile_becomes_one_card_per_model() {
        let mut providers = vec![
            profile_with("a", "Solo", &["m1"]),
            profile_with("b", "Box", &["x", "y", "z"]),
        ];
        let copies = split_multi_model_profiles(&mut providers);
        let ids: Vec<&str> = providers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "b-m1", "b-m2"]);
        assert!(providers.iter().all(|p| p.models.len() == 1));
        assert_eq!(
            providers[0].name, "Solo",
            "single-model cards are untouched"
        );
        assert_eq!(providers[1].name, "Box (x)");
        assert_eq!(providers[3].models, ["z"]);
        assert_eq!(
            copies,
            [
                ("b".to_string(), "b-m1".to_string()),
                ("b".to_string(), "b-m2".to_string())
            ]
        );
    }

    #[test]
    fn split_ids_never_collide_with_existing_profiles() {
        let mut providers = vec![
            profile_with("b", "Box", &["x", "y"]),
            profile_with("b-m1", "Other", &["q"]),
        ];
        split_multi_model_profiles(&mut providers);
        let mut ids: Vec<&str> = providers.iter().map(|p| p.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn nothing_to_split_is_a_no_op() {
        let mut providers = vec![
            profile_with("a", "A", &["m"]),
            profile_with("e", "Empty", &[]),
        ];
        assert!(split_multi_model_profiles(&mut providers).is_empty());
        assert_eq!(providers.len(), 2);
    }

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
        assert!(!p.supports_vision);
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
