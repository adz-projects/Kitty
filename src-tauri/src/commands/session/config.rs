//! Per-session configuration: approval mode, thinking effort, provider
//! rebinding, and the chat/agentic mode override.

use tauri::{AppHandle, Manager};

use super::{resolve_cwd, SessionInfo, ThinkingEffort};
use crate::state::AppState;

/// Switch the session's approval mode. BigTiny has no ACP-style modes
/// handshake — HITL policy is enforced daemon-side and `new_session`
/// advertises no modes — so this is a no-op kept only so the frontend's
/// existing call site has something to call.
#[tauri::command]
pub async fn set_mode(_app: AppHandle, session_id: String, mode_id: String) -> Result<(), String> {
    let _ = (&session_id, &mode_id);
    Ok(())
}

/// Set the active session's thinking/reasoning effort. Persists the choice
/// (so a resumed session keeps it), PATCHes it into the daemon session's
/// metadata for the agent loop to translate per dialect next turn, then
/// returns the effort recomputed against the active provider — which also
/// drops a value the current provider doesn't offer. `Ok(None)` when the
/// active provider has no effort control (the UI then hides the dropdown).
#[tauri::command]
pub async fn set_thinking_effort(
    app: AppHandle,
    session_id: String,
    value: String,
) -> Result<Option<ThinkingEffort>, String> {
    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        cfg.session_efforts.insert(session_id.clone(), value.clone());
        crate::config::save(&cfg).map_err(|e| e.to_string())?;
    }
    crate::bigtiny::sessions::update_thinking_effort(&app, &session_id, &value).await?;
    Ok(crate::bigtiny::effort::thinking_effort_for(&app, &session_id))
}

/// Read the effort control for a session, recomputed against the active
/// provider. The frontend calls this when the provider or model changes
/// mid-session so the dropdown appears/disappears/re-scopes without a reload.
#[tauri::command]
pub async fn get_thinking_effort(
    app: AppHandle,
    session_id: String,
) -> Result<Option<ThinkingEffort>, String> {
    Ok(crate::bigtiny::effort::thinking_effort_for(&app, &session_id))
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

/// "Return to thought partner" — repoint the session back to a fresh private
/// per-chat folder (the default, no-project state). The inverse of
/// `set_session_context_dir`: it hands the session a new `<base>/chats/…`
/// directory so `is_default_folder` becomes true again, and returns the
/// refreshed `SessionInfo` for the chat header to re-render from.
#[tauri::command]
pub async fn reset_session_context_dir(
    app: AppHandle,
    session_id: String,
) -> Result<SessionInfo, String> {
    let cwd = resolve_cwd(&app).await?;
    crate::bigtiny::sessions::reset_cwd(&app, &session_id, cwd).await
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

