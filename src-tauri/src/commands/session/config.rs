//! Per-session configuration: thinking effort, provider rebinding, working
//! folders and persona.

use tauri::{AppHandle, Manager};

use super::{resolve_cwd, SessionInfo, ThinkingEffort};
use crate::state::AppState;

/// Set a chat's reasoning effort. Persists the choice (so a resumed chat
/// keeps it), PATCHes it into the engine session's metadata for the agent
/// loop to translate per dialect next turn, then returns the effort
/// recomputed against the chat's own card. `Ok(None)` when that card's model
/// has no effort control (the UI then hides the picker).
#[tauri::command]
pub async fn set_thinking_effort(
    app: AppHandle,
    session_id: String,
    value: String,
) -> Result<Option<ThinkingEffort>, String> {
    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        cfg.session_efforts
            .insert(session_id.clone(), value.clone());
        crate::config::save(&cfg).map_err(|e| e.to_string())?;
    }
    crate::bigtiny::sessions::update_thinking_effort(&app, &session_id, &value).await?;
    Ok(crate::bigtiny::effort::thinking_effort_for_session(&app, &session_id).await)
}

/// Read the effort control for a chat, resolved against the chat's own card.
/// The frontend calls this when the chat's card changes, or a detection for
/// it lands, so the picker appears/disappears/re-scopes without a reload.
#[tauri::command]
pub async fn get_thinking_effort(
    app: AppHandle,
    session_id: String,
) -> Result<Option<ThinkingEffort>, String> {
    Ok(crate::bigtiny::effort::thinking_effort_for_session(&app, &session_id).await)
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
/// The folders this session may reach because the user granted them: its own
/// chat folder, every working folder set during the session, and anything
/// attached. Backs the working-directory pill's hover list.
#[tauri::command]
pub async fn list_session_allowed_dirs(
    app: AppHandle,
    session_id: String,
) -> Result<serde_json::Value, String> {
    crate::bigtiny::sessions::allowed_dirs(&app, &session_id).await
}

/// Withdraw one granted path. The pill's hover list is the only place this is
/// reachable, which is deliberate: an allowance the user cannot see is one they
/// cannot knowingly withdraw.
#[tauri::command]
pub async fn revoke_session_dir(
    app: AppHandle,
    session_id: String,
    path: String,
) -> Result<(), String> {
    crate::bigtiny::sessions::revoke_dir(&app, &session_id, &path).await
}

#[tauri::command]
pub async fn reset_session_context_dir(
    app: AppHandle,
    session_id: String,
) -> Result<SessionInfo, String> {
    let cwd = resolve_cwd(&app).await?;
    crate::bigtiny::sessions::reset_cwd(&app, &session_id, cwd).await
}
