//! Kitty's one listener on the daemon's per-app event stream
//! (`GET /api/apps/me/events`).
//!
//! A chat's own send stream only reaches the window that started the turn,
//! and only while it is open. Things that happen outside that - an approval
//! for a chat no window is showing, a scheduled run nobody is watching -
//! arrive here instead, once, for every session Kitty owns. Started after the
//! first attach and kept running for the life of the app: it reconnects with
//! backoff, re-attaching to whichever daemon `bigtiny::client` currently
//! points at, and after every (re)connect it collects any approval that
//! paused while it was not listening.
//!
//! The daemon sends a keepalive comment every 15s, so a stream that goes
//! quiet for [`SILENCE_LIMIT`] is treated as dead and reconnected.

use std::sync::atomic::Ordering;
use std::time::Duration;

use bigtiny2_client::sse::{drain_complete_frames, parse_frame};
use bigtiny2_protocol::{SSEEvent, SSEEventType};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use crate::approvals::{self, Decision, PendingApproval};
use crate::state::AppState;

/// Three missed keepalives.
const SILENCE_LIMIT: Duration = Duration::from_secs(45);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Start the listener, once per process. Safe to call on every attach.
pub fn ensure_running(app: &AppHandle) {
    let state = app.state::<AppState>();
    if state.app_events_started.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            match listen_once(&app).await {
                Ok(()) => backoff = Duration::from_secs(1),
                Err(e) => tracing::debug!("app event stream: {e}"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    });
}

/// One connection, until it ends or goes silent. `Ok` when it was connected
/// at all, so the next attempt starts from the short backoff.
async fn listen_once(app: &AppHandle) -> Result<(), String> {
    let client = crate::bigtiny::client::ensure_client(app)?;
    let resp = client
        .request_stream(reqwest::Method::GET, "/api/apps/me/events")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("the engine answered {}", resp.status()));
    }
    // Anything that paused while nobody was listening.
    recover_pending(app).await;

    let mut bytes = resp.bytes_stream();
    let mut buffer: Vec<u8> = Vec::new();
    let mut scan_from = 0usize;
    loop {
        let chunk = match tokio::time::timeout(SILENCE_LIMIT, bytes.next()).await {
            Err(_) => return Err("no keepalive; reconnecting".into()),
            Ok(None) => return Ok(()),
            Ok(Some(Err(e))) => return Err(e.to_string()),
            Ok(Some(Ok(chunk))) => chunk,
        };
        buffer.extend_from_slice(&chunk);
        for frame in drain_complete_frames(&mut buffer, &mut scan_from) {
            if let Some(frame) = parse_frame(&frame) {
                handle(app, frame.event).await;
            }
        }
    }
}

async fn handle(app: &AppHandle, event: SSEEvent) {
    match event.event_type {
        SSEEventType::HitlPause => {
            let (Some(action_id), Some(session_id)) = (event.action_id, event.session_id) else {
                return;
            };
            on_pause(
                app,
                action_id,
                session_id,
                event.tool_name.unwrap_or_default(),
                event.tool_args.unwrap_or(Value::Null),
            )
            .await;
        }
        SSEEventType::ScheduleRun => schedule_run(app, &event),
        SSEEventType::HitlResolved => {
            let Some(action_id) = event.action_id else {
                return;
            };
            let timed_out = event.error_type.as_deref() == Some("approval_timeout");
            resolved(app, &action_id, event.session_id.as_deref(), timed_out);
        }
        _ => {}
    }
}

/// A scheduled run started or ended. The settings list refreshes either way;
/// a run that failed, or had to skip tools nobody approved in time, is
/// worth a notification (decision: failures only), opening the run's chat.
fn schedule_run(app: &AppHandle, event: &SSEEvent) {
    let _ = app.emit("scheduled_tasks://changed", ());
    let name = event.tool_name.as_deref().unwrap_or("A scheduled task");
    let (title, body) = match event.content.as_deref() {
        Some("failed") => (
            format!("{name} failed"),
            event
                .error_message
                .clone()
                .unwrap_or_else(|| "The scheduled run did not finish.".to_string()),
        ),
        Some("denied_by_timeout") => (
            format!("{name} skipped some steps"),
            "It needed an approval nobody gave in time, so those tool calls were skipped."
                .to_string(),
        ),
        _ => return,
    };
    crate::notifications::notify_if_hidden(
        app,
        crate::notifications::Event::TaskFailed,
        &title,
        &body,
        event.session_id.as_deref(),
    );
}

/// Every approval the daemon holds for Kitty, run through [`on_pause`] as if
/// it had just arrived. Already-known ones are skipped there.
pub(crate) async fn recover_pending(app: &AppHandle) {
    let Ok(client) = crate::bigtiny::client::ensure_client(app) else {
        return;
    };
    let Ok(listing) = client.get_json("/api/apps/me/pending").await else {
        return;
    };
    let items = listing
        .get("pending")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    for item in items {
        let text = |key: &str| item.get(key).and_then(|v| v.as_str()).map(str::to_string);
        if let (Some(action_id), Some(session_id)) = (text("action_id"), text("session_id")) {
            on_pause(
                app,
                action_id,
                session_id,
                text("tool_name").unwrap_or_default(),
                item.get("tool_args").cloned().unwrap_or(Value::Null),
            )
            .await;
        }
    }
}

/// Answer an approval request if it is safe to, otherwise put it in front of
/// a person.
async fn on_pause(
    app: &AppHandle,
    action_id: String,
    session_id: String,
    tool_name: String,
    tool_args: Value,
) {
    if app
        .state::<AppState>()
        .pending_approvals
        .lock()
        .unwrap()
        .contains_key(&action_id)
    {
        return;
    }
    let session = session_view(app, &session_id).await;
    let warning = match approvals::decide(&tool_args, &session.dirs) {
        Decision::Allow => {
            match approve(app, &session_id, &action_id, "allow", None).await {
                Ok(()) => return,
                // Could not answer it: a person will have to.
                Err(e) => Some(format!("Kitty could not approve this automatically: {e}")),
            }
        }
        Decision::Prompt { warning } => Some(warning),
    };

    let pending = PendingApproval {
        always_scope: approvals::always_scope(&tool_name, &tool_args),
        action_id: action_id.clone(),
        session_id: session_id.clone(),
        tool_name: tool_name.clone(),
        tool_args: tool_args.clone(),
        warning,
        scheduled: session.scheduled,
    };
    app.state::<AppState>()
        .pending_approvals
        .lock()
        .unwrap()
        .insert(action_id.clone(), pending.clone());
    crate::notifications::refresh_tray(app);
    let _ = app.emit("approval://needed", &pending);
    // The chat's own inline prompt, in the shape the chat view has always
    // listened for.
    let _ = app.emit(
        "chat://tool-approval-needed",
        json!({
            "session_id": session_id,
            "tool_call_id": action_id,
            "tool_call": {
                "toolCallId": action_id,
                "title": tool_name,
                "kind": "execute",
                "rawInput": tool_args,
            },
            "options": crate::bigtiny::stream::approval_options(),
        }),
    );
    bring_to_attention(app, &pending);
}

/// Somebody has to see this: a notification if they are elsewhere, and on
/// desktop, when no Kitty window is showing at all, the overlay (where the
/// approval dialog appears) - decision #7.
fn bring_to_attention(app: &AppHandle, pending: &PendingApproval) {
    let title = if pending.scheduled {
        "A scheduled task needs your approval"
    } else {
        "Approval needed"
    };
    crate::notifications::notify_if_hidden(
        app,
        crate::notifications::Event::ApprovalNeeded,
        title,
        &format!("Kitty wants to run {}", pending.tool_name),
        Some(&pending.session_id),
    );
    #[cfg(desktop)]
    if !crate::windows::any_kitty_window_visible(app) {
        if let Err(e) = crate::windows::show_overlay(app) {
            tracing::warn!("could not show the overlay for an approval: {e}");
        }
    }
}

/// An approval was answered (anywhere) or ran out of time.
fn resolved(app: &AppHandle, action_id: &str, session_id: Option<&str>, timed_out: bool) {
    app.state::<AppState>()
        .pending_approvals
        .lock()
        .unwrap()
        .remove(action_id);
    crate::notifications::refresh_tray(app);
    let _ = app.emit(
        "approval://resolved",
        json!({ "action_id": action_id, "session_id": session_id, "timed_out": timed_out }),
    );
}

/// What the approval policy needs to know about a session.
struct SessionView {
    /// Everything it may touch: its folders and attached files.
    dirs: Vec<String>,
    /// A scheduled run's session.
    scheduled: bool,
}

async fn session_view(app: &AppHandle, session_id: &str) -> SessionView {
    let dirs = match crate::bigtiny::sessions::allowed_dirs(app, session_id).await {
        Ok(v) => grant_dirs(&v),
        Err(e) => {
            tracing::warn!("could not read {session_id}'s folders for an approval: {e}");
            Vec::new()
        }
    };
    let scheduled = match crate::bigtiny::client::ensure_client(app) {
        Ok(client) => client
            .get_json(&format!("/api/chat/{session_id}"))
            .await
            .ok()
            .and_then(|s| session_metadata(&s))
            .is_some_and(|m| m.get("schedule_id").is_some_and(|v| !v.is_null())),
        Err(_) => false,
    };
    SessionView { dirs, scheduled }
}

/// A session row's `metadata`, which the daemon returns as a JSON string or
/// an object depending on the route.
fn session_metadata(session: &Value) -> Option<Value> {
    let meta = session
        .get("metadata")
        .or_else(|| session.get("session").and_then(|s| s.get("metadata")))?;
    match meta {
        Value::String(s) => serde_json::from_str(s).ok(),
        Value::Object(_) => Some(meta.clone()),
        _ => None,
    }
}

/// `GET /api/chat/{id}/allowed_dirs` flattened: chat folder, working folder,
/// every folder it has worked in, and every attached file.
fn grant_dirs(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = ["chat_dir", "cwd"]
        .iter()
        .filter_map(|k| v.get(*k).and_then(|d| d.as_str()).map(str::to_string))
        .collect();
    for key in ["working_dirs", "attached_paths"] {
        if let Some(list) = v.get(key).and_then(|l| l.as_array()) {
            out.extend(list.iter().filter_map(|d| d.as_str()).map(str::to_string));
        }
    }
    out
}

/// `POST /api/chat/{id}/approve`.
pub(crate) async fn approve(
    app: &AppHandle,
    session_id: &str,
    action_id: &str,
    decision: &str,
    args_pattern: Option<&str>,
) -> Result<(), String> {
    let client = crate::bigtiny::client::ensure_client(app)?;
    let mut body = json!({ "action_id": action_id, "decision": decision });
    if let Some(pattern) = args_pattern {
        body["args_pattern"] = Value::String(pattern.to_string());
    }
    client
        .post_json(&format!("/api/chat/{session_id}/approve"), &body)
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_are_flattened_in_full() {
        let v = json!({
            "chat_dir": "C:/chats/c1",
            "cwd": "D:/proj",
            "working_dirs": ["D:/old"],
            "attached_paths": ["E:/a.pdf"],
        });
        assert_eq!(
            grant_dirs(&v),
            ["C:/chats/c1", "D:/proj", "D:/old", "E:/a.pdf"]
        );
        assert!(grant_dirs(&json!({})).is_empty());
    }

    #[test]
    fn metadata_is_read_whether_stringified_or_not() {
        let as_string = json!({"metadata": "{\"schedule_id\":\"j1\"}"});
        let as_object = json!({"session": {"metadata": {"schedule_id": "j1"}}});
        assert_eq!(session_metadata(&as_string).unwrap()["schedule_id"], "j1");
        assert_eq!(session_metadata(&as_object).unwrap()["schedule_id"], "j1");
    }
}
