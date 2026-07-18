//! Provider profiles (Phase 5). Profile *metadata* lives in app config; secrets
//! live only in the Windows Credential Manager via `keyring` — never on disk in
//! plaintext (CLAUDE.md rule 4). Activating a profile routes goosed to that
//! provider by injecting Goose's env vars when we (re)spawn `goose serve`.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};

use crate::config::Config;
use crate::lifecycle::ollama_proc;
use crate::state::AppState;

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
    #[serde(default)]
    pub created_at: String,
}

impl ProviderProfile {
    pub fn network_tier(&self) -> NetworkTier {
        network_tier_for(&self.base_url)
    }
}

/// If the active provider is an Ollama profile with a chosen model, return its
/// `(base_url, model)` — used to warm/evict the model in Ollama's memory
/// (Round-2 item 5). `None` for non-Ollama or model-less profiles.
pub fn active_ollama_target(config: &Config) -> Option<(String, String)> {
    let active = config
        .active_provider_id
        .as_ref()
        .and_then(|id| config.providers.iter().find(|p| &p.id == id))?;
    if active.provider_type != "ollama" {
        return None;
    }
    let model = active.models.first()?.clone();
    Some((active.base_url.clone(), model))
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
    let host = host_of(&p.base_url);
    let _ = app.emit(
        "provider://health",
        json!({ "reachable": reachable, "host": host, "name": p.name }),
    );
}

/// Lightweight, on-demand connectivity+auth probe for a provider profile —
/// never used for a background poll (that was deliberately removed, see
/// `emit_health_from_send_result`'s doc comment above); only called from
/// `activate_provider` (reject a switch to a non-functioning provider) and
/// the manual "Retry connection check" command. `Ok(())` means the profile
/// looks usable; `Err(String)` is a human-readable reason to show the user.
pub async fn test_connection(profile: &ProviderProfile) -> Result<(), String> {
    match profile.provider_type.as_str() {
        "ollama" => {
            let client = crate::util::http_client();
            if !ollama_proc::probe_version(&client, &profile.base_url).await {
                return Err(format!("couldn't reach Ollama at {}", profile.base_url));
            }
            if let Some(model) = profile.models.first() {
                if !ollama_proc::has_model_tag(&client, &profile.base_url, model).await {
                    return Err(format!(
                        "Ollama is reachable, but \"{model}\" isn't installed"
                    ));
                }
            }
            Ok(())
        }
        "openrouter" => {
            let key = get_secret_async(&profile.id)
                .await
                .ok_or("no API key stored for this profile — edit it and add one")?;
            crate::openrouter::get_credits(&key).await.map(|_| ())
        }
        "anthropic" => {
            let key = get_secret_async(&profile.id)
                .await
                .ok_or("no API key stored for this profile — edit it and add one")?;
            let client = crate::util::http_client();
            let url = format!("{}/v1/models", profile.base_url.trim_end_matches('/'));
            let resp = client
                .get(url)
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01")
                .timeout(Duration::from_secs(10))
                .send()
                .await
                .map_err(|e| format!("could not reach Anthropic: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!(
                    "Anthropic returned {} — check the API key",
                    resp.status()
                ));
            }
            Ok(())
        }
        "openai" | "custom_openai" => {
            let client = crate::util::http_client();
            let url = format!("{}/models", profile.base_url.trim_end_matches('/'));
            let mut req = client.get(url).timeout(Duration::from_secs(10));
            if let Some(key) = get_secret_async(&profile.id).await {
                req = req.bearer_auth(key);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| format!("could not reach {}: {e}", profile.base_url))?;
            if !resp.status().is_success() {
                return Err(format!("{} returned {}", profile.base_url, resp.status()));
            }
            Ok(())
        }
        other => Err(format!("unknown provider type: {other}")),
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

/// Same as [`get_secret`], but off the async runtime's worker thread — use
/// this from `async fn`/`#[tauri::command] async fn` bodies. Windows
/// Credential Manager access is synchronous OS IPC; calling `get_secret`
/// directly from an async context blocks that tokio worker (and everything
/// else scheduled on it) for however long the OS call takes.
pub async fn get_secret_async(id: &str) -> Option<String> {
    let id = id.to_string();
    tokio::task::spawn_blocking(move || get_secret(&id))
        .await
        .ok()
        .flatten()
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

/// Map our provider_type to Goose's `GOOSE_PROVIDER` value. `pub(crate)` so
/// `commands::session::rebind_session_provider` can reuse the exact same
/// mapping when hot-rebinding an already-open session's provider/model via
/// `session/set_config_option`, rather than duplicating it.
pub(crate) fn goose_provider_name(provider_type: &str) -> &str {
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
                // OpenRouter has no dedicated native client in goosed — it's
                // an OpenAI-compatible endpoint, and goosed's OpenAI-family
                // client is what actually sends the request regardless of
                // `GOOSE_PROVIDER`'s label. Confirmed by a real failure:
                // switching to an OpenRouter profile mid-chat sent the
                // request to the OpenAI client's own *default* base URL
                // (`api.openai.com`) with no key at all — i.e. goosed read
                // neither `OPENROUTER_API_KEY` nor anything OpenRouter-
                // specific for the actual HTTP call; it needed the standard
                // `OPENAI_*` names. Set both families so this works
                // regardless of which one goosed's OpenRouter handling
                // actually reads.
                if let Some(s) = secret {
                    env.push(("OPENROUTER_API_KEY".into(), s.clone()));
                    env.push(("OPENAI_API_KEY".into(), s));
                }
                // goosed's OpenAI-compatible client appends `/v1/chat/completions`
                // onto whatever base URL it's given — but OpenRouter's own
                // canonical base URL (`DEFAULT_URL.openrouter` /
                // https://openrouter.ai/api/v1) already ends in `/v1`.
                // Passing it through unstripped produced a real, confirmed
                // failure: a doubled path, `.../api/v1/v1/chat/completions`
                // (404). Strip the trailing `/v1` before handing it to the
                // OPENAI_* vars, which expect a bare base with no `/v1` of
                // its own.
                let openai_base = active.base_url.trim_end_matches('/');
                let openai_base = openai_base.strip_suffix("/v1").unwrap_or(openai_base);
                env.push(("OPENAI_BASE_URL".into(), openai_base.to_string()));
                env.push(("OPENAI_HOST".into(), openai_base.to_string()));
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

    // Global (not per-provider) context-management strategy (Round-4 item 3).
    env.push((
        "GOOSE_CONTEXT_STRATEGY".into(),
        config.context_strategy.clone(),
    ));

    // Round-5: nudge the model to save generated files into the session's own
    // working directory (the per-chat `Documents/Kitty/chats/<id>/` folder,
    // which goose already sets as the shell cwd) instead of writing to an
    // absolute path like goose's built-in `~/Documents/Goose` default. Injected
    // every turn via the bundled `tom` (Top Of Mind) platform extension's
    // `GOOSE_MOIM_MESSAGE_TEXT` env. Relative-path writes already land in the
    // chat folder; this is a soft nudge to make the model prefer them for
    // documents/spreadsheets it would otherwise dump in ~/Documents/Goose.
    //
    // `GOOSE_MOIM_MESSAGE_TEXT` is a single scalar env (goosed has no way to
    // combine multiple `tom` messages), so every "nudge the model every turn"
    // need has to append here rather than push its own separate env entry —
    // a second `env.push(("GOOSE_MOIM_MESSAGE_TEXT", ...))` would just
    // silently clobber this one.
    let mut moim_message = String::from(
        "When you create, generate, or export any file (documents, spreadsheets, scripts, \
         data, etc.), always save it into the current working directory using a relative \
         path such as `report.docx`. Do not write to an absolute path such as \
         ~/Documents/Goose — the working directory is already set to the correct folder \
         for this conversation.",
    );

    // Adaptive Pathway's `decide`/`record_outcome` MCP tools rely entirely on
    // the model reading `decide`'s own tool description ("call this before
    // selecting any tool") and choosing to comply on its own — there's no ACP
    // mechanism to inject the extension's `adaptive_instructions` MCP prompt
    // (session/new silently drops systemPrompt/instructions, confirmed in
    // docs/acp-protocol.md's "Recipes / skills" section). In practice this
    // signal is too easy to miss for a small/local model choosing among many
    // other enabled extensions' tools — confirmed live: a real Kitty install
    // with several genuine conversations had a completely empty
    // adaptive-pathway database (zero rows in every table) despite the
    // extension being enabled and registered. Fold the same reminder into the
    // `tom` nudge every turn instead of leaving it to chance.
    if config.adaptive_pathway_enabled {
        // `AP_EMBED_OLLAMA_MODEL` lives on goosed's own process env here so the
        // extension registration in `AdaptivePathway.tsx` can declare it as an
        // `env_keys` entry (goose forwards named env vars from *its own*
        // environment to the spawned MCP child — the extension-registration
        // `env` param only declares names, it cannot carry a literal value
        // directly). This is how the sidecar (spawned separately by Kitty,
        // see `adaptive_pathway_proc::ensure_running`) and this MCP extension
        // (spawned by goosed) end up pinned to the same embedding model tag
        // without either one's config drifting from the other.
        env.push((
            "AP_EMBED_OLLAMA_MODEL".into(),
            config.adaptive_pathway_embedding_model.clone(),
        ));
        env.push(("AP_EMBED_OLLAMA_URL".into(), config.ollama_base_url.clone()));

        // Kept short and load-bearing-instruction-first deliberately: this
        // whole block is one shared scalar re-injected into every turn (see
        // the file-path nudge above), and a small/local model is more likely
        // to skip an instruction buried after several paragraphs of preamble
        // than one it sees immediately. Kitty also auto-records a
        // best-effort `record_outcome` from the ACP tool-call stream now
        // (see `goosed::stream::track_and_maybe_record_outcome`) as a
        // backstop for exactly this — but the model isn't told that, since
        // its own call carries real `context` the backstop can't provide,
        // and knowing a backstop exists would only invite skipping the
        // higher-quality signal.
        moim_message.push_str(
            " Before choosing a tool, call the adaptive-pathway extension's `decide` tool \
             with the tool(s) you're considering, and pass a `context` parameter: a short \
             one-sentence summary of what this conversation is about (e.g. \"reviewing a \
             novel draft\") — this keeps what you learn in one topic from leaking into \
             suggestions for a different one. After using a tool, call `record_outcome` \
             with the tool and reward (1.0 success, -1.0 failure, 0.0 neutral) — every time, \
             not just occasionally.",
        );

        // Adaptive Pathway also learns general response-style preferences, not
        // just tool selection — e.g. what kind of writing critique the user
        // finds useful. `decide`/`record_annotation` are fully generic (plain
        // string labels, no tool-call assumption), and Kitty's own hint-badge
        // UI already renders 👍👎 feedback buttons for any `decide` call that
        // returns hints, regardless of what the labels mean — so this reuses
        // the exact same mechanism as the tool nudge above, just with
        // different labels and record_annotation instead of record_outcome
        // (a style preference isn't a pass/fail execution result).
        moim_message.push_str(
            " Adaptive Pathway also learns HOW you respond, not just which tool you use. \
             Before a substantive response with a real choice of approach — critique, \
             explanations, brainstorming, planning (not simple factual answers) — call \
             `decide` with a few candidate `style:...` labels as the tools you're \
             considering, e.g. `style:critique:structural`, `style:critique:direct-tone`, \
             `style:explain:concise`, `style:proactive:ask-first` — invent new ones as \
             needed. Use the top hint to shape your response. Afterward, watch the user's \
             next message for a reaction and call `record_annotation` on the label you \
             used: clear positive signals (thanks, building on it, direct confirmation, \
             acting on it without pushback) mean `keep_this` (or `micro_positive` if \
             milder); clear negative signals (correction, re-asking, pushback, redoing it \
             themselves) mean `dont_do_again` (or `micro_negative` if milder). No clear \
             reaction — don't annotate.",
        );

        // Scenario this guards against: a response covers two angles (e.g. a
        // mainstream, well-supported option and an alternative one); the user
        // reacts positively to just the mainstream one, but without this
        // instruction the model could annotate BOTH as liked, rewarding an
        // option the user never actually endorsed. Merged with the intensity
        // rule below since both are short caveats on the same mechanism.
        moim_message.push_str(
            " Attribution matters: only annotate the specific label your reaction was \
             about — if a response combined multiple approaches, do not also annotate the \
             others just because they appeared in the same response; skip annotating if \
             you're not sure which one it was about. For `dont_do_again`, use intensity 0.9 \
             (not the normal ~0.5) when the user clearly wants a topic stopped going \
             forward (e.g. \"stop suggesting that\") — that suppresses it from suggestions \
             for about a month instead of a soft nudge.",
        );
    }

    env.push(("GOOSE_MOIM_MESSAGE_TEXT".into(), moim_message));

    env
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
        assert_eq!(p.context_length, None);
        assert_eq!(p.models, vec!["llama3.2:3b"]);
        assert!(!p.strip_reasoning);
        assert_eq!(p.system_prompt, None);
        assert_eq!(p.prompt_idle_timeout_secs, None);
    }

    fn moim_text(env: &[(String, String)]) -> &str {
        env.iter()
            .find(|(k, _)| k == "GOOSE_MOIM_MESSAGE_TEXT")
            .map(|(_, v)| v.as_str())
            .expect("GOOSE_MOIM_MESSAGE_TEXT must always be present")
    }

    #[test]
    fn goosed_env_sets_single_moim_message_text_entry() {
        // A second env.push(("GOOSE_MOIM_MESSAGE_TEXT", ...)) would silently
        // clobber the first when goosed reads its env — every "nudge the
        // model every turn" need must append to one shared string instead.
        let cfg = Config::default();
        let env = goosed_env(&cfg);
        let count = env
            .iter()
            .filter(|(k, _)| k == "GOOSE_MOIM_MESSAGE_TEXT")
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn goosed_env_openrouter_also_sets_openai_base_url() {
        // Goose has no dedicated native client for OpenRouter — it's the
        // OpenAI-compatible client under the hood, which silently falls back
        // to api.openai.com when OPENAI_BASE_URL isn't set. Confirmed by a
        // real failure: switching to an OpenRouter profile mid-chat sent the
        // request to OpenAI's own default endpoint with no key at all.
        let cfg = Config {
            providers: vec![ProviderProfile {
                id: "p1".into(),
                name: "OpenRouter".into(),
                provider_type: "openrouter".into(),
                base_url: "https://openrouter.ai/api/v1".into(),
                models: vec!["some/model".into()],
                is_trusted: true,
                temperature: None,
                top_p: None,
                context_length: None,
                strip_reasoning: false,
                system_prompt: None,
                prompt_idle_timeout_secs: None,
                created_at: String::new(),
            }],
            active_provider_id: Some("p1".into()),
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        // The trailing `/v1` must be stripped — goosed's OpenAI-compatible
        // client appends `/v1/chat/completions` itself, and passing the raw
        // (already-`/v1`-suffixed) base through unstripped produced a real,
        // confirmed doubled-path 404: `.../api/v1/v1/chat/completions`.
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "OPENAI_BASE_URL")
                .map(|(_, v)| v.as_str()),
            Some("https://openrouter.ai/api")
        );
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "OPENAI_HOST")
                .map(|(_, v)| v.as_str()),
            Some("https://openrouter.ai/api")
        );
    }

    #[test]
    fn goosed_env_openrouter_strips_v1_regardless_of_trailing_slash() {
        let cfg = Config {
            providers: vec![ProviderProfile {
                id: "p1".into(),
                name: "OpenRouter".into(),
                provider_type: "openrouter".into(),
                base_url: "https://openrouter.ai/api/v1/".into(),
                models: vec!["some/model".into()],
                is_trusted: true,
                temperature: None,
                top_p: None,
                context_length: None,
                strip_reasoning: false,
                system_prompt: None,
                prompt_idle_timeout_secs: None,
                created_at: String::new(),
            }],
            active_provider_id: Some("p1".into()),
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "OPENAI_BASE_URL")
                .map(|(_, v)| v.as_str()),
            Some("https://openrouter.ai/api")
        );
    }

    #[test]
    fn moim_message_includes_adaptive_pathway_nudge_when_enabled() {
        let cfg = Config {
            adaptive_pathway_enabled: true,
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        let text = moim_text(&env);
        assert!(
            text.contains("relative"),
            "file-path nudge must still be present"
        );
        assert!(text.contains("decide"));
        assert!(text.contains("record_outcome"));
    }

    #[test]
    fn goosed_env_sets_embedding_model_when_adaptive_pathway_enabled() {
        // The MCP extension (spawned by goosed) can't receive a literal env
        // value straight from `add_extension` — only `env_keys` naming a var
        // goosed already has in *its own* environment. This entry is that var.
        let cfg = Config {
            adaptive_pathway_enabled: true,
            adaptive_pathway_embedding_model: "qwen3-embedding:0.6b".into(),
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "AP_EMBED_OLLAMA_MODEL")
                .map(|(_, v)| v.as_str()),
            Some("qwen3-embedding:0.6b")
        );
    }

    #[test]
    fn goosed_env_omits_embedding_model_when_adaptive_pathway_disabled() {
        let cfg = Config {
            adaptive_pathway_enabled: false,
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        assert!(!env.iter().any(|(k, _)| k == "AP_EMBED_OLLAMA_MODEL"));
    }

    #[test]
    fn goosed_env_sets_embedding_url_when_adaptive_pathway_enabled() {
        let cfg = Config {
            adaptive_pathway_enabled: true,
            ollama_base_url: "http://localhost:11434".into(),
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "AP_EMBED_OLLAMA_URL")
                .map(|(_, v)| v.as_str()),
            Some("http://localhost:11434")
        );
    }

    #[test]
    fn goosed_env_omits_embedding_url_when_adaptive_pathway_disabled() {
        let cfg = Config {
            adaptive_pathway_enabled: false,
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        assert!(!env.iter().any(|(k, _)| k == "AP_EMBED_OLLAMA_URL"));
    }

    #[test]
    fn moim_message_omits_adaptive_pathway_nudge_when_disabled() {
        let cfg = Config {
            adaptive_pathway_enabled: false,
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        let text = moim_text(&env);
        assert!(
            text.contains("relative"),
            "file-path nudge must still be present"
        );
        assert!(!text.contains("decide"));
        assert!(!text.contains("record_outcome"));
    }

    #[test]
    fn moim_message_includes_response_style_nudge_when_enabled() {
        let cfg = Config {
            adaptive_pathway_enabled: true,
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        let text = moim_text(&env);
        assert!(text.contains("style:critique:structural"));
        assert!(text.contains("record_annotation"));
        assert!(text.contains("keep_this"));
        assert!(text.contains("dont_do_again"));
        // Both nudges must coexist in the single scalar env, not clobber each other.
        assert!(
            text.contains("record_outcome"),
            "tool nudge must still be present"
        );
    }

    #[test]
    fn moim_message_omits_response_style_nudge_when_disabled() {
        let cfg = Config {
            adaptive_pathway_enabled: false,
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        let text = moim_text(&env);
        assert!(!text.contains("style:critique:structural"));
        assert!(!text.contains("record_annotation"));
    }

    #[test]
    fn moim_message_includes_context_param_instruction_when_enabled() {
        let cfg = Config {
            adaptive_pathway_enabled: true,
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        let text = moim_text(&env);
        assert!(text.contains("`context` parameter"));
        // Without this, preferences bleed across unrelated topics purely
        // from call frequency (the frequency-obsession scenario).
        assert!(text.contains("leaking into"));
    }

    #[test]
    fn moim_message_includes_attribution_precision_instruction_when_enabled() {
        let cfg = Config {
            adaptive_pathway_enabled: true,
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        let text = moim_text(&env);
        // Guards the mixed-response misattribution scenario: liking one of
        // several angles a response covered must not reward all of them.
        assert!(text.contains("Attribution matters"));
        assert!(text.contains("do not also annotate the others"));
    }

    #[test]
    fn moim_message_omits_new_instructions_when_disabled() {
        let cfg = Config {
            adaptive_pathway_enabled: false,
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        let text = moim_text(&env);
        assert!(!text.contains("`context` parameter"));
        assert!(!text.contains("Attribution matters"));
        assert!(!text.contains("intensity 0.9"));
    }

    #[test]
    fn moim_message_includes_dont_do_again_intensity_guidance_when_enabled() {
        let cfg = Config {
            adaptive_pathway_enabled: true,
            ..Config::default()
        };
        let env = goosed_env(&cfg);
        let text = moim_text(&env);
        // Guards the topic-lock-in scenario: a clear "stop showing me this"
        // must use high intensity so the engine's TTL suppression fires.
        assert!(text.contains("intensity 0.9"));
        assert!(text.contains("suppresses it from suggestions for about a month"));
    }
}
