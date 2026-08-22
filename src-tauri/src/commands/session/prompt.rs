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
