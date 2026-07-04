//! Dispatch of incoming ACP frames: match responses to pending requests, map
//! `session/update` notifications to `chat://*` Tauri events, and answer
//! server-initiated requests. See docs/acp-protocol.md for the frame shapes.

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::goosed::api::Pending;

pub async fn handle_incoming(
    app: &AppHandle,
    out: &mpsc::UnboundedSender<Message>,
    pending: &Pending,
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
        (true, true) => handle_server_request(app, out, &v).await,
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
        // usage_update / available_commands_update / current_mode_update / plan:
        // not surfaced in Phase 2.
        _ => {}
    }
}

/// Answer a server-initiated request. Phase 3 wires the real approval UI; for
/// now we auto-cancel permission prompts (never sent in `auto` mode) and reject
/// filesystem callbacks we didn't advertise support for.
async fn handle_server_request(app: &AppHandle, out: &mpsc::UnboundedSender<Message>, v: &Value) {
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");

    let response = match method {
        "session/request_permission" => {
            // Surface it (so Phase 3 UI can already observe), then cancel.
            let session_id = v
                .pointer("/params/sessionId")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let _ = app.emit(
                "chat://tool-approval-needed",
                json!({ "session_id": session_id, "params": v.get("params") }),
            );
            json!({ "jsonrpc": "2.0", "id": id, "result": { "outcome": { "outcome": "cancelled" } } })
        }
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("method not supported: {method}") }
        }),
    };

    let _ = out.send(Message::Text(response.to_string()));
}
