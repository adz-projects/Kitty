//! Per-session configuration: approval mode, thinking effort, provider
//! rebinding, and the chat/agentic mode override.

use serde_json::json;
use tauri::{AppHandle, Manager};

use crate::config;
use crate::config::providers;
use crate::goosed::api;
use crate::state::AppState;

use super::{parse_thinking_effort, ThinkingEffort};

/// Switch the session's approval mode (`auto` / `approve` / `smart_approve`).
#[tauri::command]
pub async fn set_mode(app: AppHandle, session_id: String, mode_id: String) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    client
        .request(
            "session/set_mode",
            json!({ "sessionId": session_id, "modeId": mode_id }),
        )
        .await?;
    Ok(())
}

/// Set the active session's thinking/reasoning effort (ACP
/// `session/set_config_option`, live-probed — `configId`, not `key`/`option`,
/// is the required field; see docs/acp-protocol.md). Live, per-session, no
/// goosed restart needed — unlike provider/temperature/model, which are
/// spawn-time env vars.
#[tauri::command]
pub async fn set_thinking_effort(
    app: AppHandle,
    session_id: String,
    value: String,
) -> Result<Option<ThinkingEffort>, String> {
    let client = api::ensure_client(&app).await?;
    let result = client
        .request(
            "session/set_config_option",
            json!({ "sessionId": session_id, "configId": "thinking_effort", "value": value }),
        )
        .await?;
    Ok(parse_thinking_effort(&result))
}

/// Hot-rebind an *already-open* session onto the currently-active provider's
/// model, via the same `session/set_config_option` mechanism (confirmed live,
/// `docs/acp-protocol.md`: `configOptions` includes `provider`/`model` select
/// entries, settable exactly like `thinking_effort` above).
///
/// Switching providers today only respawns goosed with new env vars —
/// correct for a brand-new session (`GOOSE_PROVIDER`/`GOOSE_MODEL` become its
/// default), but confirmed real bug: an *already-loaded* session keeps its
/// own previously-bound model, so continuing to chat in the same session
/// after switching sent the OLD provider's model id to the NEW provider
/// ("... is not a valid model ID"). This call is best-effort and swallows its
/// own failures — the `session/set_config_option` value format for
/// `provider`/`model` isn't independently live-probed beyond the
/// `thinking_effort` precedent, so if it's ever rejected, the worst case is
/// simply no rebind (today's existing behavior), never a new visible error.
#[tauri::command]
pub async fn rebind_session_provider(app: AppHandle, session_id: String) {
    let (provider_value, model_value) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        let active = cfg
            .active_provider_id
            .as_ref()
            .and_then(|id| cfg.providers.iter().find(|p| &p.id == id));
        match active {
            Some(p) => (
                Some(providers::goose_provider_name(&p.provider_type).to_string()),
                p.models.first().cloned(),
            ),
            None => (None, None),
        }
    };
    let Some(provider_value) = provider_value else {
        return;
    };
    let Ok(client) = api::ensure_client(&app).await else {
        return;
    };
    let _ = client
        .request(
            "session/set_config_option",
            json!({ "sessionId": session_id, "configId": "provider", "value": provider_value }),
        )
        .await;
    if let Some(model_value) = model_value {
        let _ = client
            .request(
                "session/set_config_option",
                json!({ "sessionId": session_id, "configId": "model", "value": model_value }),
            )
            .await;
    }
}

/// Get a session's persisted chat/agentic mode override, if any (`None` =
/// default to chat — see `Config::session_modes`'s doc comment).
#[tauri::command]
pub fn get_session_mode(
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> Result<Option<String>, String> {
    let cfg = state.config.lock().unwrap();
    Ok(cfg.session_modes.get(&session_id).cloned())
}

/// Set (or clear, via `None`) a session's mode override.
#[tauri::command]
pub fn set_session_mode(
    state: tauri::State<'_, AppState>,
    session_id: String,
    mode: Option<String>,
) -> Result<(), String> {
    let mut cfg = state.config.lock().unwrap();
    match mode {
        Some(m) if !m.trim().is_empty() => {
            cfg.session_modes.insert(session_id, m);
        }
        _ => {
            cfg.session_modes.remove(&session_id);
        }
    }
    config::save(&cfg).map_err(|e| e.to_string())
}
