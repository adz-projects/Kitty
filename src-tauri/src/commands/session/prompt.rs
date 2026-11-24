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
///
/// `text` is only what the user typed. `documents` (inlined pastes and text
/// drops) and `attached_paths` are laid out around it here
/// (`bigtiny::turn_text::compose`), the same code that takes them apart
/// again when the chat is resumed.
// Each parameter is a field of the webview's `send_prompt` call.
#[allow(clippy::too_many_arguments)]
pub async fn send_prompt(
    app: AppHandle,
    session_id: String,
    text: String,
    images: Option<Vec<ImageAttachment>>,
    attached_paths: Option<Vec<String>>,
    documents: Option<Vec<crate::bigtiny::turn_text::InlineDocument>>,
    debate: Option<bool>,
    research: Option<crate::bigtiny::research::ResearchArgs>,
    critique: Option<String>,
    tidy: Option<bool>,
) -> Result<(), String> {
    // `/debate`: a moderator brief rides as the turn's last document, and the
    // panel it names is armed in the engine for this turn only.
    let mut documents = documents.unwrap_or_default();
    let debate_panel = if debate.unwrap_or(false) {
        let turn = crate::bigtiny::debate::prepare(
            &app,
            &session_id,
            &documents,
            attached_paths.as_deref().unwrap_or_default(),
        )
        .await;
        documents.push(turn.brief);
        turn.panel
    } else {
        None
    };
    // `/critique`: the editor's playbook rides with the first turn; the
    // user's answer to its brief is armed with the panel the lenses run on.
    let debate_panel = match critique
        .as_deref()
        .and_then(crate::bigtiny::critique::Stage::parse)
    {
        Some(stage) => {
            let turn = crate::bigtiny::critique::prepare(
                &app,
                &session_id,
                stage,
                &text,
                &documents,
                attached_paths.as_deref().unwrap_or_default(),
            )
            .await;
            documents.extend(turn.playbook);
            turn.panel
        }
        None => debate_panel,
    };
    // `/deepresearch`: the engine ties this chat to a new run and returns turn
    // 0's instructions, which ride with the brief so this turn drafts the
    // plan the user then approves.
    if let Some(args) = research {
        let client = crate::bigtiny::client::ensure_client(&app)?;
        let started = crate::bigtiny::research::start(&client, &session_id, &text, &args)
            .await
            .map_err(|e| {
                tracing::warn!("deep research start for {session_id}: {e}");
                if e.contains("already has a research run") {
                    "This chat already has a deep research run in progress.".to_string()
                } else {
                    "Could not start deep research.".to_string()
                }
            })?;
        documents.push(crate::bigtiny::research::instructions_document(&started));
    }
    // `/tidy`: Kitty grades the chats, ties the candidates to this chat, and
    // the list rides with the user's wishes so this turn proposes a tidy.
    if tidy.unwrap_or(false) {
        let doc = crate::bigtiny::tidy::prepare(&app, &session_id, &text)
            .await
            .map_err(|e| {
                tracing::warn!("tidy prepare for {session_id}: {e}");
                // An engine left running from before /tidy existed.
                if e.contains("answered 404") {
                    "Kitty's engine is older than this Kitty and has no /tidy yet. It updates \
                     when it restarts: on its own once nothing is using it, or from the banner."
                        .to_string()
                } else {
                    "Could not read the chats to tidy.".to_string()
                }
            })?;
        documents.push(doc);
    }
    let text = crate::bigtiny::turn_text::compose(
        &text,
        &documents,
        attached_paths.as_deref().unwrap_or_default(),
    );
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
        // The context-length confirmation rides the same gate: the daemon's
        // wrap-up valve reads the model's context window off the provider
        // row, and a row without one leaves it budgeting against the daemon's
        // 64k default instead of the model's real window. Same first-turn
        // moment, same write-only-when-changed rule as the effort
        // confirmation. The three write different provider fields, so they
        // run together rather than one after another.
        tokio::join!(
            crate::bigtiny::effort::confirm_model_effort(&app, &session_id),
            crate::bigtiny::context_window::confirm_model_context_length(&app, &session_id),
            crate::bigtiny::vision::confirm_model_vision(&app, &session_id),
        );
    }
    crate::bigtiny::stream::send_prompt(app, session_id, text, images, attached_paths, debate_panel)
        .await
}

/// "Regenerate" on the chat's latest answer: ask its last question again, in
/// the same chat. Refused while the chat is replying.
#[tauri::command]
pub async fn regenerate_last(app: AppHandle, session_id: String) -> Result<(), String> {
    crate::bigtiny::sessions::regenerate_last(&app, &session_id).await
}

/// Watch a turn running elsewhere (a specialist's chat): replays it so far,
/// then follows it live. `false` when nothing is running there.
#[tauri::command]
pub async fn attach_session_stream(app: AppHandle, session_id: String) -> Result<bool, String> {
    crate::bigtiny::stream::attach_session_stream(&app, &session_id).await
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
/// `approvals::always_scope`), `reject`, or `revise`. `message` is the
/// person's own words for the model: what to change (required for `revise`),
/// or why it was rejected.
#[tauri::command]
pub async fn answer_approval(
    app: AppHandle,
    action_id: String,
    decision: String,
    message: Option<String>,
) -> Result<(), String> {
    if !matches!(
        decision.as_str(),
        "allow" | "always_allow" | "reject" | "revise"
    ) {
        return Err(format!("unknown decision: {decision}"));
    }
    if decision == "revise" && message.as_deref().map(str::trim).unwrap_or("").is_empty() {
        return Err("Say what should change.".into());
    }
    crate::bigtiny::stream::answer_approval(&app, &action_id, &decision, message.as_deref()).await
}

/// The whole output of a tool call whose card showed a truncated result.
#[tauri::command]
pub async fn fetch_full_tool_result(
    app: AppHandle,
    session_id: String,
    tool_call_id: String,
) -> Result<String, String> {
    crate::bigtiny::stream::fetch_full_tool_result(&app, &session_id, &tool_call_id).await
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
    client
        .delete(&format!("/api/hitl/rules/{id}"))
        .await
        .map(|_| ())
}
