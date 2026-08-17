//! Provider profile commands: list/create/update/delete + activation (which
//! re-registers the provider with BigTiny and warms/evicts local Ollama
//! models around the switch).

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::config;
use crate::config::providers::{self, NetworkTier, ProviderProfile};
use crate::config::Config;
use crate::openrouter;
use crate::state::AppState;

/// A provider profile plus derived fields the UI needs.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderView {
    #[serde(flatten)]
    pub profile: ProviderProfile,
    pub network_tier: NetworkTier,
    pub has_secret: bool,
    pub active: bool,
}

/// Async — `has_secret` is a blocking Windows Credential Manager IPC call per
/// profile, so this runs off the main thread and offloads each lookup through
/// `get_secret_async` (a `list_providers` call with many profiles would
/// otherwise block the command thread for the full round of OS dialogs).
async fn provider_views(cfg: &Config) -> Vec<ProviderView> {
    let mut views = Vec::with_capacity(cfg.providers.len());
    for p in &cfg.providers {
        let has_secret = providers::get_secret_async(&p.id).await.is_some();
        views.push(ProviderView {
            network_tier: p.network_tier(),
            has_secret,
            active: cfg.active_provider_id.as_deref() == Some(&p.id),
            profile: p.clone(),
        });
    }
    views
}

/// List provider profiles with derived tier / secret / active flags.
#[tauri::command]
pub async fn list_providers(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<ProviderView>, String> {
    // Snapshot the config out of the lock — the async secret lookups below
    // must not hold the global config Mutex across their awaited OS calls.
    let cfg = state.config.lock().unwrap().clone();
    Ok(provider_views(&cfg).await)
}

/// Create or update a provider profile. `secret`, when present, is stored in the
/// keyring only (never in config.json). Returns the saved profile (with id).
///
/// If the edited profile is the **currently active** provider, the change is
/// re-synced to BigTiny immediately (no restart / reactivate required) so
/// settings like `context_length` take effect on the next chat turn. Best-effort
/// on the daemon round-trip: the profile is already persisted, and a transient
/// daemon problem shouldn't fail the save — rebind/activate will re-sync later.
#[tauri::command]
pub async fn upsert_provider(
    app: AppHandle,
    mut profile: ProviderProfile,
    secret: Option<String>,
) -> Result<ProviderProfile, String> {
    if profile.id.trim().is_empty() {
        profile.id = format!("prof_{}", chrono::Utc::now().timestamp_millis());
    }
    if profile.created_at.trim().is_empty() {
        profile.created_at = chrono::Utc::now().to_rfc3339();
    }
    if let Some(s) = secret {
        if !s.is_empty() {
            // Async write: `set_secret` is blocking Windows Credential
            // Manager IPC, which must not run on a tokio worker.
            providers::set_secret_async(&profile.id, &s).await?;
        }
    }
    let is_active;
    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        match cfg.providers.iter_mut().find(|p| p.id == profile.id) {
            Some(existing) => *existing = profile.clone(),
            None => cfg.providers.push(profile.clone()),
        }
        config::save(&cfg).map_err(|e| e.to_string())?;
        is_active = cfg.active_provider_id.as_deref() == Some(profile.id.as_str());
    }

    if is_active {
        if let Err(e) = crate::bigtiny::providers::sync_active_provider(&app).await {
            tracing::warn!(
                "provider {} edited but failed to re-sync to BigTiny: {e}",
                profile.id
            );
        }
    }
    Ok(profile)
}

/// Delete a provider profile (and its stored secret).
///
/// `async` for a platform reason, not a performance one: a *synchronous*
/// `#[tauri::command]` runs on the main thread, and on Android the secret
/// store is reached by posting to the main looper and waiting for the reply
/// (`android::secrets`). A sync command touching a secret therefore deadlocks
/// the app. Everything else that reads or writes one is already async; this
/// was the last sync holdout.
#[tauri::command]
pub async fn delete_provider(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    providers::delete_secret(&id);
    let mut cfg = state.config.lock().unwrap();
    cfg.providers.retain(|p| p.id != id);
    if cfg.active_provider_id.as_deref() == Some(&id) {
        cfg.active_provider_id = None;
    }
    config::save(&cfg).map_err(|e| e.to_string())
}

/// Best-effort context-length lookup for OpenRouter models, for the Providers
/// form's auto-suggest (Round-6 Feature 1). `Ok(None)` (not `Err`) when the
/// model isn't found in the list — this is a suggestion, not a required value.
#[tauri::command]
pub async fn openrouter_context_length(model: String) -> Result<Option<u32>, String> {
    let models = openrouter::list_models().await?;
    Ok(openrouter::context_length_for(&models, &model))
}

/// Best-effort context-length lookup for a **remote** Ollama server, via
/// `POST /api/show`. The Ollama case lost its live lookup when managed Ollama
/// was retired (commit `8c5fbef`); this restores it for the endpoint the user
/// runs themselves. `Ok(None)` (never `Err` on a shape we don't recognize)
/// keeps the field manually editable — this only ever *suggests* a value.
#[tauri::command]
pub async fn ollama_context_length(
    base_url: String,
    model: String,
) -> Result<Option<u32>, String> {
    let url = format!("{}/api/show", base_url.trim_end_matches('/'));
    let resp = crate::util::http_client()
        .post(url)
        .json(&serde_json::json!({ "model": model }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("could not reach Ollama: {e}"))?;
    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(context_length_from_show(&json))
}

/// Pull `<arch>.context_length` out of an Ollama `/api/show` `model_info`
/// block. The architecture prefix varies per model (`llama.`, `qwen2.`,
/// `gemma3.`, …), so match on the `.context_length` suffix rather than
/// hardcoding a family. Split out pure so it's unit-testable without a live
/// server, mirroring `providers::connection::tags_response_has_tag`.
fn context_length_from_show(json: &serde_json::Value) -> Option<u32> {
    json.get("model_info")
        .and_then(|m| m.as_object())
        .and_then(|obj| {
            obj.iter()
                .find(|(k, _)| k.ends_with(".context_length"))
                .and_then(|(_, v)| v.as_u64())
        })
        .and_then(|n| u32::try_from(n).ok())
}

/// Best-effort context-length lookup for a `custom_openai` server (release-
/// fixes item 15) — unlike Ollama there's no one API every such server
/// implements, so this tries two shapes in order and returns `Ok(None)`
/// (never `Err`) the moment neither one is recognized, keeping the field
/// manually editable exactly like the Ollama/OpenRouter suggestions above:
///
/// 1. `GET /props` — llama.cpp server's own endpoint; several field names
///    have been used across versions (`n_ctx`, `n_ctx_train`,
///    `default_generation_settings.n_ctx`), so all three are tried.
/// 2. `GET /v1/models` — the OpenAI-compatible listing. Real OpenAI carries
///    no context-length field, but several self-hosted servers (LM Studio
///    and others) add a non-standard `context_length` per entry in `data`.
#[tauri::command]
pub async fn custom_openai_context_length(
    base_url: String,
    model: String,
) -> Result<Option<u32>, String> {
    let base = base_url.trim_end_matches('/');
    let client = crate::util::http_client();

    if let Ok(resp) = client
        .get(format!("{base}/props"))
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
    {
        if let Ok(json) = resp.json::<serde_json::Value>().await {
            if let Some(n) = context_length_from_props(&json) {
                return Ok(Some(n));
            }
        }
    }

    if let Ok(resp) = client
        .get(format!("{base}/v1/models"))
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
    {
        if let Ok(json) = resp.json::<serde_json::Value>().await {
            if let Some(n) = context_length_from_models_list(&json, &model) {
                return Ok(Some(n));
            }
        }
    }

    Ok(None)
}

fn context_length_from_props(json: &serde_json::Value) -> Option<u32> {
    for path in [
        &["n_ctx"][..],
        &["n_ctx_train"][..],
        &["default_generation_settings", "n_ctx"][..],
    ] {
        let mut cur = json;
        let mut ok = true;
        for key in path {
            match cur.get(key) {
                Some(v) => cur = v,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            if let Some(n) = cur.as_u64().and_then(|n| u32::try_from(n).ok()) {
                return Some(n);
            }
        }
    }
    None
}

fn context_length_from_models_list(json: &serde_json::Value, model: &str) -> Option<u32> {
    json.get("data")?.as_array()?.iter().find_map(|entry| {
        let id = entry.get("id").and_then(|v| v.as_str())?;
        if id != model {
            return None;
        }
        entry
            .get("context_length")
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok())
    })
}

/// Check an OpenRouter provider profile's current credit balance/usage.
/// Reads the key from the keyring (never sent to/stored by Kitty otherwise)
/// — errors if the profile has no stored secret or isn't an OpenRouter profile.
#[tauri::command]
pub async fn openrouter_credits(
    state: tauri::State<'_, AppState>,
    provider_id: String,
) -> Result<serde_json::Value, String> {
    let profile = {
        let cfg = state.config.lock().unwrap();
        cfg.providers
            .iter()
            .find(|p| p.id == provider_id)
            .cloned()
            .ok_or("no such provider profile")?
    };
    if profile.provider_type != "openrouter" {
        return Err("not an OpenRouter profile".into());
    }
    let key = providers::get_secret_async(&provider_id)
        .await
        .ok_or("no API key stored for this profile — edit it and add one")?;
    openrouter::get_credits(&key).await
}

/// Manual, user-triggered re-check of the active provider (the chat view's
/// "can't reach" banner's Retry button) — reuses `test_connection` rather
/// than reintroducing any background polling. `Ok(())` when there's no active
/// provider (goosed's own config) — nothing for Kitty to check in that case.
#[tauri::command]
pub async fn test_active_provider_connection(
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let profile = {
        let cfg = state.config.lock().unwrap();
        cfg.active_provider_id
            .as_ref()
            .and_then(|id| cfg.providers.iter().find(|p| &p.id == id).cloned())
    };
    match profile {
        Some(p) => providers::test_connection(&p).await,
        None => Ok(()),
    }
}

/// Activate a provider profile. BigTiny has no built-in default and errors
/// any send with no provider registered, so `id: None` is rejected — a
/// provider must always be active. Health-gates the switch first (a
/// non-functioning target is rejected and the old provider stays active —
/// see `providers::test_connection`), then persists the choice and
/// re-registers it with BigTiny over REST (no daemon restart needed).
///
/// `session_id` (optional) is the invoking window's *active session*: when
/// given, only that session is stamped with the newly-active provider/model
/// (per-session isolation). Other open windows' sessions keep theirs —
/// provider is resolved per session at send time, not globally.
#[tauri::command]
pub async fn activate_provider(
    app: AppHandle,
    id: Option<String>,
    session_id: Option<String>,
) -> Result<(), String> {
    if id.is_none() {
        return Err("A provider must be active — add one in Settings → Providers.".to_string());
    }
    if let Some(ref pid) = id {
        let profile = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
            cfg.providers.iter().find(|p| &p.id == pid).cloned()
        };
        let profile = profile.ok_or("no such provider profile")?;
        providers::test_connection(&profile)
            .await
            .map_err(|e| format!("Can't switch to {} — {e}", profile.name))?;
    }

    let stamp_pid = id.clone();
    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        if let Some(ref pid) = id {
            if !cfg.providers.iter().any(|p| &p.id == pid) {
                return Err("no such provider profile".into());
            }
        }
        cfg.active_provider_id = id;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    // BigTiny switches providers at runtime over REST — no daemon restart.
    // Registration failure is a hard error.
    crate::bigtiny::providers::sync_active_provider(&app).await?;

    // Per-session stamp: apply the newly-active provider to the invoking
    // window's own open session (if it has one), so this pick never bleeds
    // into other windows' sessions. Resolved from the profile's first model,
    // the same default `sync_active_provider`/`rebind_session` use.
    if let (Some(sid), Some(pid)) = (session_id.as_deref(), stamp_pid.as_deref()) {
        let default_model = {
            let state = app.state::<AppState>();
            let cfg = state.config.lock().unwrap();
            cfg.providers
                .iter()
                .find(|p| p.id == pid)
                .and_then(|p| p.models.first().cloned())
                .unwrap_or_default()
        };
        crate::bigtiny::providers::set_session_provider(&app, sid, pid, &default_model).await;
    }

    // Tell the frontend to re-sync provider state immediately (Round-2 item 4) —
    // without this the UI drifts until the next session create/load or health tick.
    let _ = app.emit("provider://activated", ());

    // No warm/evict step any more: that existed to keep an Ollama-resident
    // model hot across a provider switch. The in-process engine's slot manager
    // owns residency now, and a remote endpoint's memory isn't ours to manage.
    Ok(())
}

/// Stamp a single session with a specific provider/model (`PATCH
/// /api/chat/{id}/config`) without touching the global active provider or
/// any other session — the per-session isolation primitive. Used when
/// resuming/restoring a session that should keep its own provider
/// independent of what's currently active.
#[tauri::command]
pub async fn set_session_provider(
    app: AppHandle,
    session_id: String,
    provider_id: String,
    model: Option<String>,
) -> Result<(), String> {
    crate::bigtiny::providers::set_session_provider(
        &app,
        &session_id,
        &provider_id,
        model.as_deref().unwrap_or(""),
    )
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn context_length_from_show_reads_the_arch_prefixed_key() {
        let body = json!({
            "model_info": {
                "general.architecture": "llama",
                "llama.context_length": 131072,
                "llama.embedding_length": 4096
            }
        });
        assert_eq!(context_length_from_show(&body), Some(131072));
    }

    #[test]
    fn context_length_from_show_handles_a_different_family_prefix() {
        // The prefix is the model's architecture, not always "llama".
        let body = json!({ "model_info": { "qwen2.context_length": 32768 } });
        assert_eq!(context_length_from_show(&body), Some(32768));
    }

    #[test]
    fn context_length_from_show_none_when_field_absent() {
        let body = json!({ "model_info": { "general.architecture": "llama" } });
        assert_eq!(context_length_from_show(&body), None);
    }

    #[test]
    fn context_length_from_show_none_on_unexpected_shape() {
        // A server answering with something other than Ollama's shape must
        // read as "unknown", not panic.
        let body = json!({ "unexpected": "shape" });
        assert_eq!(context_length_from_show(&body), None);
    }

    #[test]
    fn context_length_from_show_none_when_value_overflows_u32() {
        // Implausible, but a garbage huge number must degrade to "unknown"
        // rather than wrapping to a small one.
        let body = json!({ "model_info": { "llama.context_length": 5_000_000_000u64 } });
        assert_eq!(context_length_from_show(&body), None);
    }
}
