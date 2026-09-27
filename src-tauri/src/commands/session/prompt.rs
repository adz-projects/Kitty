//! Turn submission: send/cancel a prompt and respond to tool-approval prompts.

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::state::AppState;

/// An image attached to a chat turn (Round-3 item 17). `data_url` is a
/// `data:<mime>;base64,<...>` string as produced by `read_file_any`. Also
/// used as the return shape of `capture_screenshot_region` (Feature 3) —
/// hence `Serialize` too, not just `Deserialize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub mime: String,
    pub data_url: String,
}

/// Send a user turn. Returns immediately; streamed output arrives via
/// `chat://*` events, and completion via `chat://complete`.
///
/// `attached_paths` are absolute paths of files the user attached to this turn
/// (drag-and-drop / paste). They're registered as the session's approval-free
/// read set so the model can open them directly — see
/// `bigtiny::stream::send_prompt`.
#[tauri::command]
pub async fn send_prompt(
    app: AppHandle,
    session_id: String,
    text: String,
    images: Option<Vec<ImageAttachment>>,
    attached_paths: Option<Vec<String>>,
) -> Result<(), String> {
    // First turn of this chat: confirm its reasoning effort against the
    // per-model memory before the prompt goes out, so the level the user is
    // looking at is the level this turn actually runs at (and becomes this
    // model's remembered default). `insert` returns true only for a session not
    // yet confirmed this run, which is what keeps this to once per chat.
    let first_turn = {
        let state = app.state::<AppState>();
        let mut confirmed = state.effort_confirmed_sessions.lock().unwrap();
        confirmed.insert(session_id.clone())
    };
    if first_turn {
        crate::bigtiny::effort::confirm_model_effort(&app, &session_id).await;
        // Rides the same gate: the daemon's wrap-up valve reads the model's
        // context window off the provider row, and a row without one leaves it
        // budgeting against the daemon's 64k default instead of the model's
        // real window. Same first-turn moment, same write-only-when-changed
        // rule as the effort confirmation above.
        crate::bigtiny::context_window::confirm_model_context_length(&app).await;
        crate::bigtiny::vision::confirm_model_vision(&app).await;
    }
    crate::bigtiny::stream::send_prompt(app, session_id, text, images, attached_paths).await
}

/// Cancel the in-flight turn for a session.
#[tauri::command]
pub async fn cancel_prompt(app: AppHandle, session_id: String) -> Result<(), String> {
    crate::bigtiny::stream::cancel(&app, &session_id).await
}

/// Whether `session_id` currently has a turn in flight — checked fresh (not a
/// client-cached snapshot) so a window adopting the session (Expand
/// mid-stream, or just resuming one another window/process is actively
/// driving) can correctly show "still working" instead of looking stalled
/// just because a resume's replay doesn't reliably convey an in-progress turn.
#[tauri::command]
pub fn is_session_busy(state: State<'_, AppState>, session_id: String) -> bool {
    state
        .in_flight_sessions
        .lock()
        .unwrap()
        .contains(&session_id)
}

/// Respond to a deferred tool-approval prompt. `option_id` = the chosen
/// option (e.g. `allow_once`, `reject_once`); `None` cancels.
#[tauri::command]
pub async fn respond_permission(
    app: AppHandle,
    tool_call_id: String,
    option_id: Option<String>,
) -> Result<(), String> {
    crate::bigtiny::stream::respond_permission(&app, tool_call_id, option_id).await
}

/// Kept while the chat view still calls it: approvals are now noticed,
/// decided and announced in Rust (`lifecycle::app_events`), so there is
/// nothing left for this to do.
#[tauri::command]
pub fn notify_approval_needed(
    _app: AppHandle,
    _session_id: String,
    _tool_name: String,
) -> Result<(), String> {
    Ok(())
}

/// Every approval waiting on a person, across all chats - for a window that
/// opens (or expands, or resumes a chat) after the approval arrived. Asks the
/// engine too, so nothing that paused while Kitty was not listening is
/// missed.
#[tauri::command]
pub async fn list_pending_approvals(
    app: AppHandle,
) -> Result<Vec<crate::approvals::PendingApproval>, String> {
    crate::lifecycle::app_events::recover_pending(&app).await;
    let state = app.state::<crate::state::AppState>();
    let mut list: Vec<_> = state
        .pending_approvals
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect();
    list.sort_by(|a, b| a.action_id.cmp(&b.action_id));
    Ok(list)
}

/// Answer an approval: `allow`, `always_allow` (scoped, see
/// `approvals::always_scope`) or `reject`.
#[tauri::command]
pub async fn answer_approval(
    app: AppHandle,
    action_id: String,
    decision: String,
) -> Result<(), String> {
    if !matches!(decision.as_str(), "allow" | "always_allow" | "reject") {
        return Err(format!("unknown decision: {decision}"));
    }
    crate::bigtiny::stream::answer_approval(&app, &action_id, &decision).await
}

/// The "Always allow" rules Kitty has stored, for Settings → Tool
/// permissions.
#[tauri::command]
pub async fn list_allow_rules(app: AppHandle) -> Result<serde_json::Value, String> {
    let client = crate::bigtiny::client::ensure_client(&app)?;
    // A bare array: `[{id, tool_name, args_pattern, decision, created_at}]`.
    client.get_json("/api/hitl/rules").await
}

/// Revoke one stored rule: that tool (or command) asks again.
#[tauri::command]
pub async fn revoke_allow_rule(app: AppHandle, id: i64) -> Result<(), String> {
    let client = crate::bigtiny::client::ensure_client(&app)?;
    client.delete(&format!("/api/hitl/rules/{id}")).await.map(|_| ())
}
