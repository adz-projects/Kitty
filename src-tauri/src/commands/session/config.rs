//! Per-session configuration: approval mode, thinking effort, provider
//! rebinding, and the chat/agentic mode override.

use tauri::AppHandle;

use crate::config;
use crate::state::AppState;

use super::ThinkingEffort;

/// Switch the session's approval mode. BigTiny has no ACP-style modes
/// handshake — HITL policy is enforced daemon-side and `new_session`
/// advertises no modes — so this is a no-op kept only so the frontend's
/// existing call site has something to call.
#[tauri::command]
pub async fn set_mode(_app: AppHandle, session_id: String, mode_id: String) -> Result<(), String> {
    let _ = (&session_id, &mode_id);
    Ok(())
}

/// Set the active session's thinking/reasoning effort. BigTiny sessions
/// advertise no effort control (`thinking_effort: None` from `new_session`),
/// so the UI never shows the dropdown; answering `None` keeps a stale caller
/// harmless.
#[tauri::command]
pub async fn set_thinking_effort(
    _app: AppHandle,
    session_id: String,
    value: String,
) -> Result<Option<ThinkingEffort>, String> {
    let _ = (&session_id, &value);
    Ok(None)
}

/// "Set as working directory" (agentic mode only) — repoints the session's
/// *current* working directory in place via BigTiny's directory-sandboxing
/// model, instead of the old behavior of forking a brand-new session bound
/// to the dropped folder. Fixing the directory in place is what lets
/// BigTiny's sandbox allow both the session's original `chat_dir` and this
/// newly-set directory at once, rather than starting over with a session
/// that only ever knew the new folder.
#[tauri::command]
pub async fn set_session_context_dir(
    app: AppHandle,
    session_id: String,
    cwd: String,
) -> Result<(), String> {
    crate::bigtiny::sessions::update_cwd(&app, &session_id, &cwd).await
}

/// Set a session's custom/default persona (Round-6 Feature 2, re-plumbed onto
/// BigTiny's real `persona_override` mechanism instead of the old client-side
/// `<system>...</system>` text-prepend hack). Called once, from `send()`'s
/// `firstMessage` branch, before the turn's prompt goes out.
#[tauri::command]
pub async fn set_session_persona_override(
    app: AppHandle,
    session_id: String,
    persona: String,
) -> Result<(), String> {
    crate::bigtiny::sessions::update_persona_override(&app, &session_id, &persona).await
}

/// Hot-rebind an *already-open* session onto the currently-active provider's
/// model — best-effort, swallows its own failures.
#[tauri::command]
pub async fn rebind_session_provider(app: AppHandle, session_id: String) {
    crate::bigtiny::providers::rebind_session(&app, &session_id).await;
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

/// Set (or clear, via `None`) a session's mode override. Also pushes the new
/// mode to BigTiny (`bigtiny::sessions::update_mode`) so its
/// directory-sandboxing scope (`bigtiny/agent/sandbox.py`) stays in sync —
/// this is what makes flipping `ModeToggle` mid-session actually take effect
/// server-side, not just in Kitty's own `config.json`. Clearing the override
/// (`None`/empty) falls back to chat mode, matching `isChatMode`'s own
/// `modeOverride ?? 'chat'` default.
#[tauri::command]
pub async fn set_session_mode(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    session_id: String,
    mode: Option<String>,
) -> Result<(), String> {
    let effective_mode = {
        let mut cfg = state.config.lock().unwrap();
        match &mode {
            Some(m) if !m.trim().is_empty() => {
                cfg.session_modes.insert(session_id.clone(), m.clone());
            }
            _ => {
                cfg.session_modes.remove(&session_id);
            }
        }
        config::save(&cfg).map_err(|e| e.to_string())?;
        mode.filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| "chat".to_string())
    };
    if let Err(e) = crate::bigtiny::sessions::update_mode(&app, &session_id, &effective_mode).await
    {
        tracing::warn!("bigtiny mode sync failed for session {session_id}: {e}");
    }
    Ok(())
}
