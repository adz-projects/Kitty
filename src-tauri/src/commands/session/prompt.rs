//! Turn submission: send/cancel a prompt and respond to tool-approval prompts.

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

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
#[tauri::command]
pub async fn send_prompt(
    app: AppHandle,
    session_id: String,
    text: String,
    images: Option<Vec<ImageAttachment>>,
) -> Result<(), String> {
    crate::bigtiny::stream::send_prompt(app, session_id, text, images).await
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

/// Called by the frontend from `chatStore.ts`'s `onApprovalNeeded` handler,
/// only once its own `decideChatApproval` auto-decide pass has determined a
/// tool call genuinely needs a human (`decision === 'prompt'`) — never for
/// one it's about to silently auto-resolve. This is the sole trigger for an
/// "Approval needed" toast/tray-pending state: BigTiny's `hitl_pause` SSE
/// event itself no longer fires one directly (see the comment at that call
/// site in `bigtiny/stream.rs`), since under the default `always_ask` HITL
/// policy that fired for nearly every tool call, not just the ones actually
/// left waiting on a person.
#[tauri::command]
pub fn notify_approval_needed(
    app: AppHandle,
    session_id: String,
    tool_name: String,
) -> Result<(), String> {
    crate::notifications::set_tray_pending(&app, true);
    crate::notifications::notify_if_hidden(
        &app,
        crate::notifications::Event::ApprovalNeeded,
        "Approval needed",
        &format!("Kitty wants to run {tool_name}"),
        Some(&session_id),
    );
    Ok(())
}
