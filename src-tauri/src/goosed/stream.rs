//! Dispatch of incoming ACP frames: match responses to pending requests, map
//! `session/update` notifications to `chat://*` Tauri events, and answer
//! server-initiated requests. See docs/acp-protocol.md for the frame shapes.

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::goosed::api::{Pending, Perm};
use crate::notifications;

pub async fn handle_incoming(
    app: &AppHandle,
    out: &mpsc::UnboundedSender<Message>,
    pending: &Pending,
    permissions: &Perm,
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
                emit_session_update(app, &v);
            }
        }
        (false, false) => {}
    }
}

/// Map a `session/update` notification to a `chat://*` event.
fn emit_session_update(app: &AppHandle, v: &Value) {
    let params = match v.get("params") {
        Some(p) => p,
        None => return,
    };
    let session_id = params.get("sessionId").and_then(|s| s.as_str()).unwrap_or("");
    let update = match params.get("update") {
        Some(u) => u,
        None => return,
    };
    let kind = update.get("sessionUpdate").and_then(|s| s.as_str()).unwrap_or("");

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
        // Tool calls: forward the raw update; the frontend interprets shape.
        "tool_call" | "tool_call_update" => {
            let _ = app.emit(
                "chat://tool-call",
                json!({ "session_id": session_id, "phase": kind, "update": update }),
            );
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

/// Answer a server-initiated request. `session/request_permission` is *deferred*:
/// we store the JSON-RPC id keyed by the tool-call id, surface it to the UI, and
/// respond only when the user approves/denies (see `commands::respond_permission`).
/// Filesystem callbacks we didn't advertise support for get method-not-found.
async fn handle_server_request(
    app: &AppHandle,
    out: &mpsc::UnboundedSender<Message>,
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
    let _ = out.send(Message::Text(response.to_string()));
}
