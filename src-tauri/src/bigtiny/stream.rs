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
//!   `respond_permission` -> `POST .../approve`)
//! - `session_title`   -> `chat://session-title`
//! - `llm_stop`        -> captures usage for the final `chat://complete`
//! - `error`           -> `chat://error` at stream end
//! - `session_status` (`is_last`) -> `chat://complete`
//! - `compaction`      -> `chat://compaction` (background context-compaction
//!   notice; may arrive after this turn's own `chat://complete` since
//!   compaction runs fire-and-forget from the daemon's turn-`finally` block)

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
const MAX_STRING_BYTES: usize = 100 * 1024;

/// True when `s` is a `kitty-tools` visualization envelope — the
/// `{ "render_config": { "target": "iframe", … }, "html_payload": … }` shape
/// `parseIframePayload` (frontend) needs as a *complete, valid* JSON string.
/// These are never truncated: the payload is the diagram itself, bounded by
/// the viz builders' source/width budgets, and slicing it mid-document
/// corrupts the JSON, forcing the raw-`<pre>` (instead of iframe) fallback.
fn is_iframe_payload(s: &str) -> bool {
    s.contains("\"render_config\"")
        && s.contains("\"html_payload\"")
        && (s.contains("\"target\": \"iframe\"") || s.contains("\"target\":\"iframe\""))
}

/// Truncate a tool-output string for the webview event payload, marking the
/// cut — unless it is a viz iframe envelope, which must reach the webview
/// whole so `JSON.parse` can render it as an iframe.
pub(crate) fn truncate_for_ui(s: &str) -> String {
    if is_iframe_payload(s) || s.len() <= MAX_STRING_BYTES {
        return s.to_string();
    }
    let mut end = MAX_STRING_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated {} bytes]", &s[..end], s.len() - end)
}

/// Pure: byte offset of the first `"\n\n"` frame terminator in `haystack`
/// at or after byte offset `from`, or `None` if there isn't one yet.
///
/// Operates on raw bytes rather than `str::find` on a `&str` slice so the
/// caller (`drain_complete_frames`) can resume from an arbitrary byte offset
/// without needing that offset to land on a UTF-8 char boundary — `\n`
/// (0x0A) is single-byte ASCII and never appears inside a multi-byte UTF-8
/// sequence, so a boundary found here always splits *between* characters.
pub(crate) fn find_frame_boundary(haystack: &[u8], from: usize) -> Option<usize> {
    if from >= haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|rel| from + rel)
}

/// Drain every complete `\n\n`-terminated SSE frame currently in `buffer`,
/// decoding each as UTF-8. `scan_from` carries the scan's resume offset
/// between calls (see `run_stream`): without it, every incoming chunk would
/// re-scan the entire accumulated buffer from position 0 — quadratic in the
/// worst case of a large delta arriving as many small TCP chunks.
///
/// Decoding happens only once a frame is *complete* — never per TCP chunk —
/// so a multi-byte character split across chunks is decoded after all its
/// bytes have arrived, instead of being mangled per-chunk by
/// `from_utf8_lossy` into U+FFFD replacement characters the JSON parser then
/// silently rejected (dropping the whole frame). A complete frame that still
/// isn't valid UTF-8 is a daemon bug: drop it with a loud warning rather
/// than lossy-decoding; its `\n\n` has already been consumed, so the stream
/// continues cleanly from the next frame.
pub(crate) fn drain_complete_frames(buffer: &mut Vec<u8>, scan_from: &mut usize) -> Vec<String> {
    let mut frames = Vec::new();
    // `saturating_sub(1)` guards the one case a pure resume-point would
    // miss: a "\n\n" boundary split exactly across the old/new chunk join,
    // with one "\n" at the very end of the previously-scanned region and
    // the second "\n" at the very start of the new tail.
    while let Some(pos) = find_frame_boundary(buffer, scan_from.saturating_sub(1)) {
        let frame_bytes: Vec<u8> = buffer.drain(..pos + 2).collect();
        *scan_from = 0;
        match std::str::from_utf8(&frame_bytes[..frame_bytes.len() - 2]) {
            Ok(frame) => frames.push(frame.to_string()),
            Err(e) => tracing::warn!("dropping a non-UTF-8 SSE frame ({e})"),
        }
    }
    // No further boundary in the buffer yet — resume the next chunk's scan
    // from here instead of position 0.
    *scan_from = buffer.len();
    frames
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
    error_type: Option<String>,
    cancelled: bool,
    usage: Option<Value>,
    timing: Option<Value>,
}

/// Send a user turn. Returns immediately; streamed output arrives via
/// `chat://*` events and completion via `chat://complete` — the same contract
/// as the goosed `send_prompt`.
pub async fn send_prompt(
    app: AppHandle,
    session_id: String,
    text: String,
    images: Option<Vec<ImageAttachment>>,
    attached_paths: Option<Vec<String>>,
) -> Result<(), String> {
    let client = ensure_client(&app)?;

    // Register any drag-and-drop / pasted file paths as this session's
    // approval-free read set BEFORE the turn streams, so the model can open an
    // attachment directly instead of tripping a sandbox HITL approval every time
    // (the daemon unions these into `metadata.attached_paths`, which
    // `sandbox::allowed_dirs_for_session` honours). Awaited before the SSE call
    // so the allowance is in place when the tool loop computes allowed_dirs.
    let paths: Vec<String> = attached_paths
        .unwrap_or_default()
        .into_iter()
        .filter(|p| !p.trim().is_empty())
        .collect();
    if !paths.is_empty() {
        if let Err(e) = client
            .patch_json(
                &format!("/api/chat/{session_id}/config"),
                &json!({ "attached_paths": paths }),
            )
            .await
        {
            // Non-fatal: the attachment path is still named in the prompt text,
            // so at worst the model hits the old approval flow for it.
            tracing::warn!("failed to register attached paths for {session_id}: {e}");
        }
    }

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
            Ok(TurnOutcome {
                error: None,
                cancelled,
                usage,
                timing,
                ..
            }) => {
                let mut result = json!({
                    "stopReason": if cancelled { "cancelled" } else { "end_turn" },
                });
                if let Some(usage) = usage {
                    result["usage"] = usage;
                }
                if let Some(timing) = timing {
                    result["timing"] = timing;
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
                poll_compaction_status(app_bg.clone(), session_id.clone());
            }
            Ok(TurnOutcome {
                error: Some(message),
                error_type,
                ..
            }) => {
                let _ = app_bg.emit(
                    "chat://error",
                    json!({ "session_id": session_id, "message": &message, "error_type": error_type }),
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
            Err(message) => {
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
        // A restart queued behind this turn (§6.4) can now go ahead. No-op
        // unless one is actually outstanding.
        crate::lifecycle::engine_restart::apply_if_pending(&app_bg);
    });
    Ok(())
}

/// BigTiny's background compaction pass (`bigtiny/agent/loop.py`'s
/// `finally` block) runs fire-and-forget *after* a turn finishes, and
/// typically completes after this turn's own SSE stream has already
/// closed — so its `compaction` SSE event usually has no open connection
/// left to land on. Polling `/stats` once, after a short delay, and
/// diffing `compacted_through_rowid` against the last value seen for this
/// session is what actually delivers the notice to the frontend.
fn poll_compaction_status(app: AppHandle, session_id: String) {
    tauri::async_runtime::spawn(async move {
        // Give the summarizer model (small and local, but still an LLM
        // call) time to finish. Long enough to usually catch it, short
        // enough not to matter when it doesn't — the compaction indicator
        // is a nice-to-have, not correctness-critical, and a missed poll
        // is simply caught by the next turn's poll instead.
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;

        let Ok(stats) = crate::bigtiny::sessions::get_stats(&app, &session_id).await else {
            return;
        };
        let Some(rowid) = stats
            .get("compacted_through_rowid")
            .and_then(|v| v.as_i64())
        else {
            return;
        };

        let state = app.state::<AppState>();
        let mut watermarks = state.bigtiny_compaction_watermarks.lock().unwrap();
        let previous = watermarks.get(&session_id).copied().unwrap_or(0);
        if rowid > previous {
            watermarks.insert(session_id.clone(), rowid);
            drop(watermarks);
            let _ = app.emit(
                "chat://compaction",
                json!({
                    "session_id": session_id,
                    "compacted_through_rowid": rowid,
                    "memory_slots": stats.get("memory_slots"),
                }),
            );
        }
    });
}

/// The active provider profile's `prompt_idle_timeout_secs`, if any — the
/// per-provider "Response timeout" setting. Resolved from `AppState` the same
/// way `sessions::create`/`providers::emit_health_from_send_result` resolve
/// the active profile (the send-time resolution also honors a session's own
/// stamped provider on the BigTiny side; this is the app-side global fallback
/// for the idle deadline).
fn active_provider_idle_timeout(app: &AppHandle) -> Option<u32> {
    let state = app.state::<AppState>();
    let cfg = state.config.lock().unwrap();
    cfg.active_provider_id
        .as_ref()
        .and_then(|id| cfg.providers.iter().find(|p| &p.id == id))
        .and_then(|p| p.prompt_idle_timeout_secs)
        .filter(|s| *s > 0)
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
        .request_stream(
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

    // Raw-byte accumulation buffer: frames are decoded only once complete
    // (`drain_complete_frames`), so a multi-byte UTF-8 character split
    // across two TCP chunks survives intact.
    let mut buffer: Vec<u8> = Vec::new();
    let mut scan_from: usize = 0;
    // Idle-only deadline on the stream: if the daemon sends no bytes for this
    // long, the turn is wedged and we bail out (see the `Elapsed` arm). Long
    // turns that keep streaming data are unaffected. `None` -> the active
    // provider's `prompt_idle_timeout_secs`, or 300s when unset.
    let idle = std::time::Duration::from_secs(u64::from(
        active_provider_idle_timeout(app).unwrap_or(300),
    ));
    let mut bytes = resp.bytes_stream();
    'outer: loop {
        let chunk = match tokio::time::timeout(idle, bytes.next()).await {
            Ok(Some(item)) => item.map_err(|e| format!("BigTiny stream failed: {e}"))?,
            Ok(None) => break, // daemon closed the stream cleanly
            Err(_) => {
                return Err(format!(
                    "BigTiny went idle for {}s without sending data — the turn was stopped.",
                    idle.as_secs()
                ));
            }
        };
        buffer.extend_from_slice(&chunk);
        for frame in drain_complete_frames(&mut buffer, &mut scan_from) {
            let Some(event) = parse_sse_frame(&frame) else {
                continue;
            };
            let is_last = event
                .get("is_last")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
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
    let tool_name = event
        .get("tool_name")
        .and_then(|t| t.as_str())
        .unwrap_or("");

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
            let error_type = error_type_from_tool_finish(result_text);
            let failed = error_type.is_some();
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
            // End-of-turn outcome recording now lives in the BigTiny daemon
            // (`bigtiny_rust::agent::loop_::spawn_record_outcome`) where the
            // real context + reward source is available; the old app-layer
            // context-free backstop was removed to avoid double-recording the
            // same tool outcome to AP (which would skew learning rewards).
            let _ = started_name;
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
                let input = usage
                    .get("input_tokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let output = usage
                    .get("output_tokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let mut result = json!({
                    "inputTokens": input,
                    "outputTokens": output,
                    "totalTokens": input + output,
                });
                // Prompt-cache stats — absent entirely for providers/models
                // that don't report them (most local/OpenAI-compat setups),
                // as opposed to defaulting to 0 like input/output above,
                // so the frontend can tell "no cache data" apart from "cache
                // miss on every token".
                if let Some(v) = usage.get("cache_read_tokens").and_then(|v| v.as_i64()) {
                    result["cacheReadTokens"] = json!(v);
                }
                if let Some(v) = usage.get("cache_creation_tokens").and_then(|v| v.as_i64()) {
                    result["cacheCreationTokens"] = json!(v);
                }
                outcome.usage = Some(result);
            }
        }
        "llm_timing" => {
            // Fired once per LLM call within the turn (a multi-step
            // tool-calling turn makes several) — overwritten on each one, so
            // by the time the turn completes this holds the metrics for the
            // call that actually produced the final visible text.
            let get_f64 = |key: &str| event.get(key).and_then(|v| v.as_f64());
            outcome.timing = Some(json!({
                "ttfbMs": get_f64("ttfb_ms"),
                "ttftMs": get_f64("ttft_ms"),
                "generationMs": get_f64("generation_ms"),
                // Computed by the daemon, not derived here: it is the only
                // side holding both the decode window and the token count for
                // the *same* call. See `TimingResult::finalize_rate`.
                "tokensPerSecond": get_f64("tokens_per_second"),
                "totalTokens": event.get("total_tokens").and_then(|v| v.as_i64()),
            }));
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
        "provider_error" => {
            // Structured, classified provider error (context exceeded,
            // insufficient credits, etc. — see `classify_provider_error` on
            // the BigTiny side). Distinct from the generic "error" type
            // above so `chat://error` can carry `error_type` for the
            // frontend to render type-specific guidance.
            let message = event
                .get("error_message")
                .and_then(|m| m.as_str())
                .or(content)
                .unwrap_or("Provider error")
                .to_string();
            outcome.error = Some(message);
            outcome.error_type = event
                .get("error_type")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string());
        }
        "session_status" => {
            if content == Some("Cancelled") {
                outcome.cancelled = true;
            }
        }
        "compaction" => {
            // Rare in practice — the background pass usually finishes
            // after this stream has already closed (see
            // `poll_compaction_status`, the reliable delivery path) — but
            // cheap to forward directly on the lucky case it's still open.
            let _ = app.emit(
                "chat://compaction",
                json!({ "session_id": session_id, "content": content }),
            );
        }
        // model_failover / subagent_status: not surfaced yet.
        _ => {}
    }
}

/// Pure: `Some("crash")` when the tool result read as an error — used only to
/// mark the tool-call card as `failed` in the UI. (Outcome *recording* to AP
/// now lives in the BigTiny daemon, where the real context is available.)
fn error_type_from_tool_finish(result_text: &str) -> Option<&'static str> {
    if result_text.starts_with("Error") || result_text.starts_with("[Tool error") {
        Some("crash")
    } else {
        None
    }
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
    // Clone the session out rather than removing the entry up front: if the
    // `/approve` POST below fails, the approval must still be pending so the
    // caller can retry. It's only forgotten once the daemon has accepted it.
    let session_id = app
        .state::<AppState>()
        .bigtiny_approvals
        .lock()
        .unwrap()
        .get(&tool_call_id)
        .cloned()
        .ok_or("that approval request is no longer pending")?;
    let decision = decision_for_option(option_id.as_deref());
    let client = ensure_client(app)?;
    client
        .post_json(
            &format!("/api/chat/{session_id}/approve"),
            &json!({ "action_id": tool_call_id, "decision": decision }),
        )
        .await?;
    app.state::<AppState>()
        .bigtiny_approvals
        .lock()
        .unwrap()
        .remove(&tool_call_id);
    notifications::set_tray_pending(app, false);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_frame_boundary_finds_terminator() {
        assert_eq!(find_frame_boundary(b"data: a\n\ndata: b\n\n", 0), Some(7));
        assert!(find_frame_boundary(b"data: a", 0).is_none());
    }

    #[test]
    fn find_frame_boundary_resumes_from_offset() {
        let buf = b"data: a\n\ndata: b\n\n";
        // Resuming from just past the first boundary finds the second one,
        // not the first again.
        assert_eq!(find_frame_boundary(buf, 9), Some(16));
    }

    #[test]
    fn find_frame_boundary_catches_split_across_saturating_sub_one() {
        // Mirrors `run_stream`'s `scan_from.saturating_sub(1)` call
        // pattern: a "\n\n" boundary split exactly across two chunks (one
        // "\n" already scanned, the second "\n" newly appended) must still
        // be found when resuming from `scan_from - 1`.
        let buf = b"data: a\n\ndata: b";
        let scan_from = buf.len(); // as if this was the end of a prior chunk
        let buf2 = b"data: a\n\ndata: b\n\n";
        assert_eq!(
            find_frame_boundary(buf2, scan_from.saturating_sub(1)),
            Some(16)
        );
    }

    #[test]
    fn find_frame_boundary_handles_empty_and_out_of_range() {
        assert!(find_frame_boundary(b"", 0).is_none());
        assert!(find_frame_boundary(b"abc", 10).is_none());
    }

    #[test]
    fn parse_sse_frame_reads_data_line() {
        let v = parse_sse_frame("data: {\"type\":\"llm_delta\",\"content\":\"hi\"}").unwrap();
        assert_eq!(v["type"], "llm_delta");
        assert_eq!(v["content"], "hi");
    }

    /// Regression (815bugs #4): a multi-byte UTF-8 character split across two
    /// TCP chunks must not be corrupted. The old code lossy-decoded each
    /// chunk independently, turning the split character into two U+FFFD
    /// replacement characters — serde_json then rejected the frame and the
    /// event was silently dropped.
    #[test]
    fn drain_complete_frames_decodes_a_multibyte_char_split_across_chunks() {
        let payload = "data: {\"type\":\"llm_delta\",\"content\":\"café\"}\n\n";
        let bytes = payload.as_bytes();
        // Split inside the two-byte 'é' (0xC3 0xA9).
        let split = bytes
            .windows(2)
            .position(|w| w == [0xC3, 0xA9])
            .expect("test payload must contain é")
            + 1;
        let mut buffer: Vec<u8> = Vec::new();
        let mut scan_from = 0usize;

        buffer.extend_from_slice(&bytes[..split]);
        assert!(
            drain_complete_frames(&mut buffer, &mut scan_from).is_empty(),
            "no frame is complete until the terminator arrives"
        );

        buffer.extend_from_slice(&bytes[split..]);
        let frames = drain_complete_frames(&mut buffer, &mut scan_from);
        assert_eq!(frames.len(), 1);
        let parsed = parse_sse_frame(&frames[0]).expect("frame must stay valid JSON");
        assert_eq!(parsed["content"], "café");
        assert!(buffer.is_empty());
    }

    #[test]
    fn drain_complete_frames_handles_multiple_frames_and_a_partial_tail() {
        let mut buffer = b"data: a\n\ndata: b\n\ndata: par".to_vec();
        let mut scan_from = 0usize;
        let frames = drain_complete_frames(&mut buffer, &mut scan_from);
        assert_eq!(frames, vec!["data: a", "data: b"]);
        assert_eq!(buffer, b"data: par");
        // The tail becomes a frame once its terminator arrives in a later chunk.
        buffer.extend_from_slice(b"tial\n\n");
        let frames = drain_complete_frames(&mut buffer, &mut scan_from);
        assert_eq!(frames, vec!["data: partial"]);
    }

    #[test]
    fn drain_complete_frames_drops_a_non_utf8_frame_but_keeps_later_ones() {
        // A daemon bug smuggling raw bytes into one frame must not poison the
        // rest of the stream: that frame is dropped (with a warning), and the
        // next frame still parses.
        let mut buffer = b"data: \xff\xfe bad\n\ndata: {\"type\":\"llm_delta\"}\n\n".to_vec();
        let mut scan_from = 0usize;
        let frames = drain_complete_frames(&mut buffer, &mut scan_from);
        assert_eq!(frames, vec!["data: {\"type\":\"llm_delta\"}"]);
        assert!(buffer.is_empty());
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
    fn truncate_for_ui_never_truncates_iframe_payload() {
        // A viz envelope larger than the cap must pass through whole — its
        // `html_payload` holds a rendered diagram, and `parseIframePayload`
        // needs the complete JSON string to render it as an iframe.
        let big_svg = format!("<svg>{}</svg>", "x".repeat(MAX_STRING_BYTES * 2));
        // Pretty (spaced) serialization, as `serde_json::to_string_pretty` produces.
        let s = format!(
            "{{\"status\":\"success\",\"render_config\":{{\"target\": \"iframe\", \"title\": \"T\"}},\"html_payload\":\"{big_svg}\"}}"
        );
        let t = truncate_for_ui(&s);
        assert_eq!(t, s);
        assert!(!t.contains("…[truncated"));
        // Compact serialization is also recognized.
        let s = format!(
            "{{\"render_config\":{{\"target\":\"iframe\"}},\"html_payload\":\"{big_svg}\"}}"
        );
        let t = truncate_for_ui(&s);
        assert_eq!(t, s);

        // A non-envelope long string is still capped.
        let plain = "y".repeat(MAX_STRING_BYTES * 2);
        let t = truncate_for_ui(&plain);
        assert!(t.len() < plain.len());
        assert!(t.contains("…[truncated"));
    }

    #[test]
    fn error_type_from_tool_finish_only_on_error_prefixes() {
        assert_eq!(error_type_from_tool_finish("file contents here"), None);
        assert_eq!(error_type_from_tool_finish(""), None);
        assert_eq!(
            error_type_from_tool_finish("Error: file not found"),
            Some("crash")
        );
        assert_eq!(
            error_type_from_tool_finish("[Tool error: timeout]"),
            Some("crash")
        );
    }
}
