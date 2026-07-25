//! Turn streaming: drive `POST /api/chat/{id}/send` (SSE) and translate
//! BigTiny's event stream into the `chat://*` Tauri events the frontend
//! already consumes from the goosed path.
//!
//! Translation table (BigTiny SSEEvent.type -> Tauri event):
//! - `llm_delta`       -> `chat://message-delta`
//! - `reasoning_delta` -> `chat://reasoning-delta`
//! - `tool_start`      -> `chat://tool-call` (phase `tool_call`)
//! - `tool_finish`     -> `chat://tool-call` (phase `tool_call_update`)
//! - `hitl_pause`      -> `chat://tool-approval-needed` (answered later via
//!                        `respond_permission` -> `POST .../approve`)
//! - `session_title`   -> `chat://session-title`
//! - `llm_stop`        -> captures usage for the final `chat://complete`
//! - `error`           -> `chat://error` at stream end
//! - `session_status` (`is_last`) -> `chat://complete`

use futures_util::StreamExt;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use crate::bigtiny::client::{ensure_client, BigTinyClient};
use crate::commands::ImageAttachment;
use crate::config::providers;
use crate::notifications;
use crate::state::AppState;

/// Same per-string cap the goosed path applies to tool outputs forwarded to
/// the webview (`goosed::stream::MAX_STRING_BYTES`).
const MAX_STRING_BYTES: usize = 16 * 1024;

/// Truncate a tool-output string for the event payload, marking the cut.
pub(crate) fn truncate_for_ui(s: &str) -> String {
    if s.len() <= MAX_STRING_BYTES {
        return s.to_string();
    }
    let mut end = MAX_STRING_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated {} bytes]", &s[..end], s.len() - end)
}

/// Pure: parse one SSE frame (`data: {...}`) into its JSON payload.
pub(crate) fn parse_sse_frame(frame: &str) -> Option<Value> {
    let data = frame
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|l| l.trim_start())
        .collect::<Vec<_>>()
        .join("");
    if data.is_empty() {
        return None;
    }
    serde_json::from_str(&data).ok()
}

/// The approval options BigTiny's HITL flow supports, in the frontend's
/// ACP-derived vocabulary (`ApprovalPrompt.tsx` renders exactly these ids).
fn approval_options() -> Value {
    json!([
        { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
        { "optionId": "allow_always", "name": "Always allow", "kind": "allow_always" },
        { "optionId": "reject_once", "name": "Reject", "kind": "reject_once" },
    ])
}

/// Pure: map a frontend approval option id onto BigTiny's decision strings.
pub(crate) fn decision_for_option(option_id: Option<&str>) -> &'static str {
    match option_id {
        Some("allow_always") => "always_allow",
        Some(o) if o.contains("allow") => "allow",
        _ => "reject",
    }
}

/// What a finished stream adds up to, for the closing `chat://complete` /
/// `chat://error` emission.
#[derive(Default)]
struct TurnOutcome {
    error: Option<String>,
    cancelled: bool,
    usage: Option<Value>,
}

/// Send a user turn. Returns immediately; streamed output arrives via
/// `chat://*` events and completion via `chat://complete` — the same contract
/// as the goosed `send_prompt`.
pub async fn send_prompt(
    app: AppHandle,
    session_id: String,
    text: String,
    images: Option<Vec<ImageAttachment>>,
) -> Result<(), String> {
    let client = ensure_client(&app)?;

    let image_blocks: Vec<Value> = images
        .unwrap_or_default()
        .iter()
        .map(|img| {
            // Strip a "data:<mime>;base64," prefix; BigTiny wants raw base64.
            let data = img
                .data_url
                .split_once(',')
                .map(|(_, b64)| b64)
                .unwrap_or(&img.data_url);
            json!({ "data": data, "mime_type": img.mime })
        })
        .collect();
    let body = if image_blocks.is_empty() {
        json!({ "message": text })
    } else {
        json!({ "message": text, "images": image_blocks })
    };

    app.state::<AppState>()
        .in_flight_sessions
        .lock()
        .unwrap()
        .insert(session_id.clone());

    let app_bg = app.clone();
    tauri::async_runtime::spawn(async move {
        let outcome = run_stream(&app_bg, &client, &session_id, &body).await;
        match outcome {
            Ok(TurnOutcome { error: None, cancelled, usage }) => {
                let mut result = json!({
                    "stopReason": if cancelled { "cancelled" } else { "end_turn" },
                });
                if let Some(usage) = usage {
                    result["usage"] = usage;
                }
                let _ = app_bg.emit(
                    "chat://complete",
                    json!({ "session_id": session_id, "result": result }),
                );
                notifications::notify_if_hidden(
                    &app_bg,
                    notifications::Event::TaskComplete,
                    "Kitty finished",
                    "Your task is complete.",
                    Some(&session_id),
                );
                providers::emit_health_from_send_result(&app_bg, true);
            }
            Ok(TurnOutcome { error: Some(message), .. }) | Err(message) => {
                let _ = app_bg.emit(
                    "chat://error",
                    json!({ "session_id": session_id, "message": &message }),
                );
                notifications::notify_if_hidden(
                    &app_bg,
                    notifications::Event::TaskFailed,
                    "Kitty ran into a problem",
                    &message,
                    Some(&session_id),
                );
                providers::emit_health_from_send_result(&app_bg, false);
            }
        }
        notifications::set_tray_pending(&app_bg, false);
        app_bg
            .state::<AppState>()
            .in_flight_sessions
            .lock()
            .unwrap()
            .remove(&session_id);
    });
    Ok(())
}

/// Drive one send stream to completion, emitting `chat://*` events as frames
/// arrive. Transport errors surface as `Err`; agent-level errors (BigTiny's
/// `error` events) as `Ok` with `outcome.error` set.
async fn run_stream(
    app: &AppHandle,
    client: &BigTinyClient,
    session_id: &str,
    body: &Value,
) -> Result<TurnOutcome, String> {
    let resp = client
        .request(
            reqwest::Method::POST,
            &format!("/api/chat/{session_id}/send"),
        )
        .json(body)
        .send()
        .await
        .map_err(|e| format!("BigTiny send failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("BigTiny error ({status}): {text}"));
    }

    let mut outcome = TurnOutcome::default();
    // Sequential tool ids: BigTiny runs tool calls one at a time within a
    // turn, so a single "current call" (id, name) suffices to pair
    // start/finish and to know what to report to the adaptive-pathway
    // backstop below.
    let mut tool_seq: u64 = 0;
    let mut current_tool: Option<(String, String)> = None;

    let mut buffer = String::new();
    let mut bytes = resp.bytes_stream();
    'outer: while let Some(chunk) = bytes.next().await {
        let chunk = chunk.map_err(|e| format!("BigTiny stream failed: {e}"))?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(pos) = buffer.find("\n\n") {
            let frame = buffer[..pos].to_string();
            buffer.drain(..pos + 2);
            let Some(event) = parse_sse_frame(&frame) else {
                continue;
            };
            let is_last = event.get("is_last").and_then(|v| v.as_bool()).unwrap_or(false);
            handle_event(
                app,
                session_id,
                &event,
                &mut outcome,
                &mut tool_seq,
                &mut current_tool,
            );
            if is_last {
                break 'outer;
            }
        }
    }
    Ok(outcome)
}

/// Translate one BigTiny SSE event into its `chat://*` emission(s).
fn handle_event(
    app: &AppHandle,
    session_id: &str,
    event: &Value,
    outcome: &mut TurnOutcome,
    tool_seq: &mut u64,
    current_tool: &mut Option<(String, String)>,
) {
    let kind = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let content = event.get("content").and_then(|c| c.as_str());
    let tool_name = event.get("tool_name").and_then(|t| t.as_str()).unwrap_or("");

    match kind {
        "llm_delta" => {
            if let Some(text) = content {
                let _ = app.emit(
                    "chat://message-delta",
                    json!({ "session_id": session_id, "text": text }),
                );
            }
        }
        "reasoning_delta" => {
            if let Some(text) = content {
                let _ = app.emit(
                    "chat://reasoning-delta",
                    json!({ "session_id": session_id, "text": text }),
                );
            }
        }
        "tool_start" => {
            *tool_seq += 1;
            let id = format!("bt-{tool_seq}");
            *current_tool = Some((id.clone(), tool_name.to_string()));
            let _ = app.emit(
                "chat://tool-call",
                json!({
                    "session_id": session_id,
                    "phase": "tool_call",
                    "update": {
                        "toolCallId": id,
                        "title": tool_name,
                        "kind": "execute",
                        "rawInput": event.get("tool_args"),
                        "_meta": { "goose": { "toolCall": {
                            "toolName": tool_name, "extensionName": "" } } },
                    },
                }),
            );
        }
        "tool_finish" => {
            // "__budget__" is BigTiny's internal step-budget bookkeeping, not
            // a real tool the user watched start — don't render it.
            if tool_name == "__budget__" {
                return;
            }
            let (id, started_name) = current_tool
                .take()
                .unwrap_or_else(|| (format!("bt-{tool_seq}"), tool_name.to_string()));
            let result_text = event
                .get("tool_result")
                .and_then(|r| r.as_str())
                .unwrap_or("");
            let reward = reward_from_tool_finish(result_text);
            let failed = reward < 0.0;
            let _ = app.emit(
                "chat://tool-call",
                json!({
                    "session_id": session_id,
                    "phase": "tool_call_update",
                    "update": {
                        "toolCallId": id,
                        "status": if failed { "failed" } else { "completed" },
                        "rawOutput": truncate_for_ui(result_text),
                    },
                }),
            );
            maybe_record_outcome(app, session_id, &started_name, reward);
        }
        "hitl_pause" => {
            let Some(action_id) = event.get("action_id").and_then(|a| a.as_str()) else {
                return;
            };
            app.state::<AppState>()
                .bigtiny_approvals
                .lock()
                .unwrap()
                .insert(action_id.to_string(), session_id.to_string());
            // Notification + tray-pending are deliberately NOT fired here —
            // BigTiny's default HITL policy asks for approval on nearly
            // every tool call, and the frontend's own `decideChatApproval`
            // auto-decide pass (`chatStore.ts`'s `onApprovalNeeded`) silently
            // resolves the overwhelming majority of them a moment after this
            // event reaches it. Firing unconditionally here notified for
            // every single tool call, not just the ones that actually needed
            // a human — see `commands::notify_approval_needed`, which the
            // frontend calls instead, only once it knows a real prompt is
            // required.
            let _ = app.emit(
                "chat://tool-approval-needed",
                json!({
                    "session_id": session_id,
                    "tool_call_id": action_id,
                    "tool_call": {
                        "toolCallId": action_id,
                        "title": tool_name,
                        "kind": "execute",
                        "rawInput": event.get("tool_args"),
                    },
                    "options": approval_options(),
                }),
            );
        }
        "hitl_resolved" => {
            notifications::set_tray_pending(app, false);
        }
        "session_title" => {
            if let Some(title) = content {
                let _ = app.emit(
                    "chat://session-title",
                    json!({ "session_id": session_id, "title": title }),
                );
            }
        }
        "llm_stop" => {
            if let Some(usage) = event.get("usage").filter(|u| u.is_object()) {
                let input = usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                let output = usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                outcome.usage = Some(json!({
                    "inputTokens": input,
                    "outputTokens": output,
                    "totalTokens": input + output,
                }));
            }
        }
        "error" => {
            let message = event
                .get("error_message")
                .and_then(|m| m.as_str())
                .or(content)
                .unwrap_or("BigTiny reported an error")
                .to_string();
            outcome.error = Some(message);
        }
        "session_status" => {
            if content == Some("Cancelled") {
                outcome.cancelled = true;
            }
        }
        // model_failover / subagent_status: not surfaced yet.
        _ => {}
    }
}

/// Tool names the bundled adaptive-pathway MCP server exposes (see
/// `plugins/adaptive-pathway/src/adaptive_pathway/mcp_server.py`) — excluded
/// from the auto-record-outcome backstop below, same rationale as the
/// goosed path's extension-name exclusion (`goosed::stream`): recording an
/// outcome for a call to `record_outcome` itself would be nonsensical.
/// Unlike the goosed path, BigTiny's `tool_start`/`tool_finish` events carry
/// no extension identifier, so the exclusion has to be by tool name instead.
const ADAPTIVE_PATHWAY_TOOL_NAMES: &[&str] = &[
    "decide",
    "record_outcome",
    "record_annotation",
    "get_state",
    "list_edges",
    "get_edge",
    "query_attribution",
    "list_domains",
    "toggle_suggestions",
    "health_check",
    "accept_nudge",
    "session_reflection",
    "resolve_schism",
];

/// Pure: whether a tool call should be tracked for the auto-record-outcome
/// backstop.
fn should_track_tool_call(tool_name: &str) -> bool {
    !ADAPTIVE_PATHWAY_TOOL_NAMES.contains(&tool_name)
}

/// Pure: a shell-style tool result's reward — `-1.0` for anything the tool
/// itself reported as an error, `1.0` otherwise. Unlike the goosed path,
/// BigTiny's `ToolResult.content` is a flat string with no `exit_code`
/// field, so a string-prefix check on the error markers BigTiny's own MCP
/// manager writes (`bigtiny/mcp/manager.py`'s `ToolResult.content` for
/// timeouts/errors, and MCP tool responses under `[Tool ... error]`/`Error`)
/// is the only failure signal available.
fn reward_from_tool_finish(result_text: &str) -> f64 {
    let failed = result_text.starts_with("Error") || result_text.starts_with("[Tool error");
    if failed { -1.0 } else { 1.0 }
}

/// Best-effort backstop so `record_outcome` doesn't depend on the model
/// remembering to call it itself — mirrors the goosed path's
/// `track_and_maybe_record_outcome`, simplified since BigTiny already hands
/// us a paired (name, reward) at `tool_finish` instead of requiring a
/// two-step id-keyed tracker across separate start/update events. Never
/// surfaces errors to the user or blocks the stream reader.
fn maybe_record_outcome(app: &AppHandle, session_id: &str, tool_name: &str, reward: f64) {
    if !should_track_tool_call(tool_name) {
        return;
    }
    let enabled = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        cfg.adaptive_pathway_enabled
    };
    if !enabled {
        return;
    }
    let base = crate::adaptive_pathway::base_url(app);
    let session_id = session_id.to_string();
    let tool_name = tool_name.to_string();
    tauri::async_runtime::spawn(async move {
        let _ =
            crate::adaptive_pathway::record_outcome(&base, &session_id, &tool_name, reward).await;
    });
}

/// Cancel the in-flight turn (`POST /api/chat/{id}/cancel`); BigTiny resolves
/// the stream with a `Cancelled` session_status.
pub async fn cancel(app: &AppHandle, session_id: &str) -> Result<(), String> {
    let client = ensure_client(app)?;
    client
        .post_json(&format!("/api/chat/{session_id}/cancel"), &json!({}))
        .await?;
    Ok(())
}

/// Answer a deferred tool approval: the `tool_call_id` the frontend echoes
/// back IS BigTiny's action id; the session it belongs to was remembered at
/// `hitl_pause` time.
pub async fn respond_permission(
    app: &AppHandle,
    tool_call_id: String,
    option_id: Option<String>,
) -> Result<(), String> {
    let session_id = app
        .state::<AppState>()
        .bigtiny_approvals
        .lock()
        .unwrap()
        .remove(&tool_call_id)
        .ok_or("that approval request is no longer pending")?;
    let decision = decision_for_option(option_id.as_deref());
    let client = ensure_client(app)?;
    client
        .post_json(
            &format!("/api/chat/{session_id}/approve"),
            &json!({ "action_id": tool_call_id, "decision": decision }),
        )
        .await?;
    notifications::set_tray_pending(app, false);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sse_frame_reads_data_line() {
        let v = parse_sse_frame("data: {\"type\":\"llm_delta\",\"content\":\"hi\"}").unwrap();
        assert_eq!(v["type"], "llm_delta");
        assert_eq!(v["content"], "hi");
    }

    #[test]
    fn parse_sse_frame_ignores_non_data_and_empty() {
        assert!(parse_sse_frame("").is_none());
        assert!(parse_sse_frame(": keepalive").is_none());
        assert!(parse_sse_frame("data: not-json").is_none());
    }

    #[test]
    fn decision_mapping_matches_bigtiny_vocabulary() {
        assert_eq!(decision_for_option(Some("allow_once")), "allow");
        assert_eq!(decision_for_option(Some("allow_always")), "always_allow");
        assert_eq!(decision_for_option(Some("reject_once")), "reject");
        assert_eq!(decision_for_option(Some("reject_always")), "reject");
        assert_eq!(decision_for_option(None), "reject"); // cancel = reject
    }

    #[test]
    fn truncate_for_ui_caps_long_strings_at_char_boundary() {
        let s = "é".repeat(MAX_STRING_BYTES); // 2 bytes each
        let t = truncate_for_ui(&s);
        assert!(t.len() < s.len());
        assert!(t.contains("…[truncated"));
        assert_eq!(truncate_for_ui("short"), "short");
    }

    #[test]
    fn reward_from_tool_finish_success_on_plain_output() {
        assert_eq!(reward_from_tool_finish("file contents here"), 1.0);
        assert_eq!(reward_from_tool_finish(""), 1.0);
    }

    #[test]
    fn reward_from_tool_finish_failure_on_error_prefixes() {
        assert_eq!(reward_from_tool_finish("Error: file not found"), -1.0);
        assert_eq!(
            reward_from_tool_finish("[Tool error: something broke]"),
            -1.0
        );
    }

    #[test]
    fn should_track_tool_call_excludes_adaptive_pathway_tools() {
        assert!(!should_track_tool_call("decide"));
        assert!(!should_track_tool_call("record_outcome"));
        assert!(!should_track_tool_call("resolve_schism"));
    }

    #[test]
    fn should_track_tool_call_includes_other_tools() {
        assert!(should_track_tool_call("shell"));
        assert!(should_track_tool_call(""));
    }
}
