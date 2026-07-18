//! Dispatch of incoming ACP frames: match responses to pending requests, map
//! `session/update` notifications to `chat://*` Tauri events, and answer
//! server-initiated requests. See docs/acp-protocol.md for the frame shapes.

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use std::time::Instant;

use crate::goosed::types::{Activity, Pending, Perm, ToolCalls};
use crate::notifications;
use crate::state::AppState;

/// Per-string cap for tool-output payloads forwarded to the webview.
const MAX_STRING_BYTES: usize = 16 * 1024;

/// Recursively cap every string in a JSON value, appending a truncation marker.
fn cap_strings(value: &Value, cap: usize) -> Value {
    match value {
        Value::String(s) if s.len() > cap => {
            let mut end = cap;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            Value::String(format!("{}…[truncated {} bytes]", &s[..end], s.len() - end))
        }
        Value::Array(arr) => Value::Array(arr.iter().map(|v| cap_strings(v, cap)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), cap_strings(v, cap)))
                .collect(),
        ),
        other => other.clone(),
    }
}

pub async fn handle_incoming(
    app: &AppHandle,
    out: &mpsc::Sender<Message>,
    pending: &Pending,
    permissions: &Perm,
    activity: &Activity,
    tool_calls: &ToolCalls,
    txt: &str,
) {
    let v: Value = match serde_json::from_str(txt) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("ACP: non-JSON frame ({e})");
            return;
        }
    };

    let has_id = v.get("id").map(|i| !i.is_null()).unwrap_or(false);
    let has_method = v.get("method").is_some();

    match (has_id, has_method) {
        // Server -> client request (must respond).
        (true, true) => handle_server_request(app, out, permissions, &v).await,
        // Response to one of our requests.
        (true, false) => {
            if let Some(id) = v.get("id").and_then(|i| i.as_i64()) {
                if let Some(tx) = pending.lock().await.remove(&id) {
                    let result = if let Some(err) = v.get("error") {
                        Err(err.clone())
                    } else {
                        Ok(v.get("result").cloned().unwrap_or(Value::Null))
                    };
                    let _ = tx.send(result);
                }
            }
        }
        // Notification.
        (false, true) => {
            if v.get("method").and_then(|m| m.as_str()) == Some("session/update") {
                // Any update at all — tool calls, thoughts, message chunks,
                // mode/title updates — counts as "this session is still
                // alive", which is what request_session_prompt's idle-reset
                // timeout keys off of (see its doc comment in goosed/api.rs).
                if let Some(sid) = v.pointer("/params/sessionId").and_then(|s| s.as_str()) {
                    activity
                        .lock()
                        .await
                        .insert(sid.to_string(), Instant::now());
                }
                emit_session_update(app, tool_calls, &v).await;
            }
        }
        (false, false) => {}
    }
}

/// Map a `session/update` notification to a `chat://*` event.
async fn emit_session_update(app: &AppHandle, tool_calls: &ToolCalls, v: &Value) {
    let params = match v.get("params") {
        Some(p) => p,
        None => return,
    };
    let session_id = params
        .get("sessionId")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let update = match params.get("update") {
        Some(u) => u,
        None => return,
    };
    let kind = update
        .get("sessionUpdate")
        .and_then(|s| s.as_str())
        .unwrap_or("");

    match kind {
        "agent_message_chunk" => {
            if let Some(text) = update.pointer("/content/text").and_then(|t| t.as_str()) {
                let _ = app.emit(
                    "chat://message-delta",
                    json!({ "session_id": session_id, "text": text }),
                );
            }
        }
        "agent_thought_chunk" => {
            if let Some(text) = update.pointer("/content/text").and_then(|t| t.as_str()) {
                let _ = app.emit(
                    "chat://reasoning-delta",
                    json!({ "session_id": session_id, "text": text }),
                );
            }
        }
        // Historical user turns, replayed by session/load (Phase 4).
        "user_message_chunk" => {
            if let Some(text) = update.pointer("/content/text").and_then(|t| t.as_str()) {
                let _ = app.emit(
                    "chat://user-message",
                    json!({ "session_id": session_id, "text": text }),
                );
            }
        }
        // Tool calls: forward the update, capping huge outputs so a giant tool
        // result can't bloat the event payload (Phase 8). Full output remains in
        // goosed and is restored on session/load replay.
        "tool_call" | "tool_call_update" => {
            let capped = cap_strings(update, MAX_STRING_BYTES);
            let _ = app.emit(
                "chat://tool-call",
                json!({ "session_id": session_id, "phase": kind, "update": capped }),
            );
            track_and_maybe_record_outcome(app, tool_calls, session_id, kind, update).await;
        }
        "session_info_update" => {
            if let Some(title) = update.get("title").and_then(|t| t.as_str()) {
                let _ = app.emit(
                    "chat://session-title",
                    json!({ "session_id": session_id, "title": title }),
                );
            }
        }
        "current_mode_update" => {
            if let Some(mode) = update.get("currentModeId").and_then(|m| m.as_str()) {
                let _ = app.emit(
                    "chat://mode",
                    json!({ "session_id": session_id, "mode": mode }),
                );
            }
        }
        // usage_update / available_commands_update / plan: not surfaced yet.
        _ => {}
    }
}

/// Extension id the adaptive-pathway MCP tools register under — used to
/// exclude the extension's own tool calls (`decide`, `record_outcome`, etc.)
/// from the auto-record-outcome backstop below (recording an outcome for a
/// call to `record_outcome` itself would be nonsensical).
const ADAPTIVE_PATHWAY_EXTENSION_ID: &str = "adaptive-pathway";

/// Pure: extract `(toolName, extensionName)` from a `tool_call` update's
/// `_meta.goose.toolCall` block, if present. Split out from
/// `track_and_maybe_record_outcome` so the ACP payload shape is unit
/// testable without async/Tauri state.
fn parse_tool_call_start(update: &Value) -> Option<(&str, &str)> {
    let tool_name = update
        .pointer("/_meta/goose/toolCall/toolName")
        .and_then(|v| v.as_str())?;
    let extension_name = update
        .pointer("/_meta/goose/toolCall/extensionName")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    Some((tool_name, extension_name))
}

/// Pure: whether a tool call from `extension_name` should be tracked for the
/// auto-record-outcome backstop — excludes the adaptive-pathway extension's
/// own tool calls (recording an outcome for a call to `record_outcome`
/// itself would be nonsensical).
fn should_track_tool_call(extension_name: &str) -> bool {
    extension_name != ADAPTIVE_PATHWAY_EXTENSION_ID
}

/// Defensive cap on the in-flight tool-call tracker, in case a call's
/// terminal `tool_call_update` never arrives (e.g. the session is abandoned
/// mid-call) — bounds memory growth over a very long-lived process. Eviction
/// is arbitrary (`HashMap` has no order), not LRU; this only needs to bound
/// size, not be precise.
const MAX_TRACKED_TOOL_CALLS: usize = 500;

/// Best-effort backstop so `record_outcome` doesn't depend on the model
/// remembering to call it: tracks each `tool_call` (name known, no status
/// yet) and fires the sidecar's `/outcome` on the completing
/// `tool_call_update` (status known, but the name isn't repeated there —
/// hence the two-step tracker keyed by `toolCallId`). Never surfaces errors
/// to the user or blocks the event-stream reader — a down/disabled sidecar
/// just means no backstop this turn, not a broken chat.
///
/// The model is still told (in the `GOOSE_MOIM_MESSAGE_TEXT` nudge) to call
/// `record_outcome` itself, since its own call carries real `context` this
/// backstop can't provide — this exists to catch what the model skips, not
/// to replace a compliant model's higher-quality signal.
async fn track_and_maybe_record_outcome(
    app: &AppHandle,
    tool_calls: &ToolCalls,
    session_id: &str,
    kind: &str,
    update: &Value,
) {
    let Some(tool_call_id) = update.get("toolCallId").and_then(|v| v.as_str()) else {
        return;
    };

    if kind == "tool_call" {
        let Some((name, extension_name)) = parse_tool_call_start(update) else {
            return;
        };
        if !should_track_tool_call(extension_name) {
            return;
        }
        let mut map = tool_calls.lock().await;
        if map.len() >= MAX_TRACKED_TOOL_CALLS {
            if let Some(oldest) = map.keys().next().cloned() {
                map.remove(&oldest);
            }
        }
        map.insert(
            tool_call_id.to_string(),
            (name.to_string(), extension_name.to_string()),
        );
        return;
    }

    // tool_call_update — a trailing title-only update (no status) means
    // "still in flight," only act on a terminal status.
    let Some(status) = update.get("status").and_then(|v| v.as_str()) else {
        return;
    };
    let tool_name = {
        let mut map = tool_calls.lock().await;
        map.remove(tool_call_id).map(|(name, _)| name)
    };
    let Some(tool_name) = tool_name else {
        return;
    };

    let enabled = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        cfg.adaptive_pathway_enabled
    };
    if !enabled {
        return;
    }

    let reward = reward_from_tool_status(status, update.get("rawOutput"));
    let base = crate::adaptive_pathway::base_url(app);
    let session_id = session_id.to_string();
    tauri::async_runtime::spawn(async move {
        let _ =
            crate::adaptive_pathway::record_outcome(&base, &session_id, &tool_name, reward).await;
    });
}

/// Pure so it's unit-testable without touching the ACP stream/tracker.
/// `"completed"` with a non-zero `rawOutput.exit_code` is still a failure —
/// a shell command can complete the ACP call while exiting non-zero, which
/// is a more precise success signal than the ACP status alone.
fn reward_from_tool_status(status: &str, raw_output: Option<&Value>) -> f64 {
    if status != "completed" {
        return -1.0;
    }
    match raw_output
        .and_then(|o| o.get("exit_code"))
        .and_then(|c| c.as_i64())
    {
        Some(code) if code != 0 => -1.0,
        _ => 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reward_completed_with_no_exit_code_is_success() {
        assert_eq!(reward_from_tool_status("completed", None), 1.0);
    }

    #[test]
    fn reward_completed_with_zero_exit_code_is_success() {
        let raw = json!({ "exit_code": 0 });
        assert_eq!(reward_from_tool_status("completed", Some(&raw)), 1.0);
    }

    #[test]
    fn reward_completed_with_nonzero_exit_code_is_failure() {
        // A shell command can "complete" the ACP call while still exiting
        // non-zero — this is a more precise signal than the ACP status alone.
        let raw = json!({ "exit_code": 1 });
        assert_eq!(reward_from_tool_status("completed", Some(&raw)), -1.0);
    }

    #[test]
    fn reward_failed_status_is_failure() {
        assert_eq!(reward_from_tool_status("failed", None), -1.0);
    }

    #[test]
    fn reward_unknown_status_is_failure() {
        assert_eq!(reward_from_tool_status("cancelled", None), -1.0);
    }

    #[test]
    fn parse_tool_call_start_extracts_name_and_extension() {
        let update = json!({
            "toolCallId": "tc1",
            "_meta": { "goose": { "toolCall": { "toolName": "shell", "extensionName": "developer" } } },
        });
        assert_eq!(parse_tool_call_start(&update), Some(("shell", "developer")));
    }

    #[test]
    fn parse_tool_call_start_defaults_missing_extension_to_empty() {
        let update = json!({
            "toolCallId": "tc1",
            "_meta": { "goose": { "toolCall": { "toolName": "shell" } } },
        });
        assert_eq!(parse_tool_call_start(&update), Some(("shell", "")));
    }

    #[test]
    fn parse_tool_call_start_none_when_tool_name_missing() {
        let update = json!({ "toolCallId": "tc1" });
        assert_eq!(parse_tool_call_start(&update), None);
    }

    #[test]
    fn should_track_tool_call_excludes_adaptive_pathway_extension() {
        assert!(!should_track_tool_call("adaptive-pathway"));
    }

    #[test]
    fn should_track_tool_call_includes_other_extensions() {
        assert!(should_track_tool_call("developer"));
        assert!(should_track_tool_call(""));
    }
}

/// Answer a server-initiated request. `session/request_permission` is *deferred*:
/// we store the JSON-RPC id keyed by the tool-call id, surface it to the UI, and
/// respond only when the user approves/denies (see `commands::respond_permission`).
/// Filesystem callbacks we didn't advertise support for get method-not-found.
async fn handle_server_request(
    app: &AppHandle,
    out: &mpsc::Sender<Message>,
    permissions: &Perm,
    v: &Value,
) {
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");

    if method == "session/request_permission" {
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        let session_id = params
            .get("sessionId")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        // Key by tool-call id (falls back to the JSON-RPC id) so the UI can
        // correlate and respond.
        let key = params
            .pointer("/toolCall/toolCallId")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| id.to_string());

        permissions.lock().await.insert(key.clone(), id);

        let title = params
            .pointer("/toolCall/title")
            .and_then(|s| s.as_str())
            .unwrap_or("a tool");
        notifications::notify_if_hidden(
            app,
            notifications::Event::ApprovalNeeded,
            "Approval needed",
            &format!("Goose wants to run {title}"),
        );
        notifications::set_tray_pending(app, true);

        let _ = app.emit(
            "chat://tool-approval-needed",
            json!({
                "session_id": session_id,
                "tool_call_id": key,
                "tool_call": params.get("toolCall"),
                "options": params.get("options"),
            }),
        );
        return;
    }

    // Anything else: we don't support it (e.g. fs/* we opted out of).
    let response = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32601, "message": format!("method not supported: {method}") }
    });
    let _ = out.send(Message::Text(response.to_string())).await;
}
