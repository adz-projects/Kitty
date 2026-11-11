//! Turn streaming: drive `POST /api/chat/{id}/send` (SSE) and translate
//! BigTiny's event stream into the `chat://*` Tauri events the frontend
//! already consumes from the goosed path.
//!
//! Translation table (BigTiny SSEEvent.type -> Tauri event):
//! - `llm_delta`       -> `chat://message-delta`
//! - `reasoning_delta` -> `chat://reasoning-delta`
//! - `tool_start`      -> `chat://tool-call` (phase `tool_call`)
//! - `tool_finish`     -> `chat://tool-call` (phase `tool_call_update`)
//! - `hitl_pause`/`hitl_resolved` -> nothing here: approvals for every
//!   session arrive once, on the app event stream (`lifecycle::app_events`),
//!   whether or not any window is streaming that session
//! - `subagent_status` -> `chat://subagent-status` (a specialist this turn
//!   delegated to started/finished/failed; carries the *child's* session id so
//!   the UI can link into its transcript)
//! - `session_title`   -> `chat://session-title`
//! - `llm_stop`        -> captures usage for the final `chat://complete`
//! - `error`           -> `chat://error` at stream end
//! - `session_status` (`is_last`) -> `chat://complete`
//! - `compaction`      -> `chat://compaction` (background context-compaction
//!   notice; may arrive after this turn's own `chat://complete` since
//!   compaction runs fire-and-forget from the daemon's turn-`finally` block)
//! - `tool_finish` for the synthetic `__context_budget__` tool
//!   -> `chat://context-budget` (the wrap-up valve withdrew this turn's tools
//!   because the model was close to its context limit)

use std::collections::HashMap;

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
    // Frames are read in place from a moving start offset, and the consumed
    // prefix is drained once at the end: draining per frame shifted the whole
    // remainder down for every frame, quadratic when one network chunk
    // carries many token frames.
    let mut start = 0usize;
    // `saturating_sub(1)` guards the one case a pure resume-point would
    // miss: a "\n\n" boundary split exactly across the old/new chunk join,
    // with one "\n" at the very end of the previously-scanned region and
    // the second "\n" at the very start of the new tail.
    let mut from = scan_from.saturating_sub(1);
    while let Some(pos) = find_frame_boundary(buffer, from) {
        match std::str::from_utf8(&buffer[start..pos]) {
            Ok(frame) => frames.push(frame.to_string()),
            Err(e) => tracing::warn!("dropping a non-UTF-8 SSE frame ({e})"),
        }
        start = pos + 2;
        from = start;
    }
    if start > 0 {
        buffer.drain(..start);
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

/// A turn that never produced an outcome: the send was refused or the stream
/// broke. `error_type` is set when the cause is one the chat can explain
/// (`turn_in_progress`, `idle_timeout`).
#[derive(Debug)]
struct StreamFailure {
    message: String,
    error_type: Option<&'static str>,
}

impl From<String> for StreamFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            error_type: None,
        }
    }
}

/// The user-facing reading of a refused `/send`. A 409 means this chat
/// already has a turn running (typically one that was stopped and has not
/// finished unwinding) - not "something went wrong".
fn send_refusal(status: reqwest::StatusCode, body: &str) -> StreamFailure {
    if status == reqwest::StatusCode::CONFLICT {
        return StreamFailure {
            message: TURN_IN_PROGRESS.to_string(),
            error_type: Some("turn_in_progress"),
        };
    }
    StreamFailure {
        message: format!("The engine refused the message ({status}): {body}"),
        error_type: None,
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

/// Hold the app process in the foreground for the lifetime of a turn on
/// Android, so an in-progress turn keeps running when the user switches apps.
///
/// On Android the whole stack — the in-process daemon future, the loopback HTTP
/// hop, every in-process MCP server — lives in the one app process, which
/// Android freezes (and may kill) minutes after it loses its last foreground
/// component. A turn-scoped `dataSync` foreground service (with its ongoing
/// notification) is what keeps that from happening. No-op on desktop, where an
/// ordinary window already keeps the process scheduled. Mirrors the download
/// equivalent (`commands::models`'s `foreground::Session`); the difference is
/// scope — this spans a turn, not a multi-GB transfer.
mod foreground {
    #[cfg(target_os = "android")]
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Turns currently holding the foreground service. The service is one
    /// instance shared by every turn, so it starts on 0→1 and stops on 1→0:
    /// the first of two overlapping turns to finish must not stop it under the
    /// other.
    #[cfg(target_os = "android")]
    static ACTIVE: AtomicUsize = AtomicUsize::new(0);

    /// Holds the turn foreground service for its lifetime, so every exit path
    /// out of the streaming task (success, error, panic) releases it without
    /// anyone having to remember to.
    pub struct TurnSession;

    impl TurnSession {
        pub fn start() -> Self {
            #[cfg(target_os = "android")]
            if ACTIVE.fetch_add(1, Ordering::SeqCst) == 0 {
                // `turn_service::start` asks for the notification permission
                // itself, once and off this thread: the turn must not wait on
                // the permission dialog.
                crate::android::turn_service::start();
            }
            TurnSession
        }
    }

    impl Drop for TurnSession {
        fn drop(&mut self) {
            #[cfg(target_os = "android")]
            if ACTIVE.fetch_sub(1, Ordering::SeqCst) == 1 {
                crate::android::turn_service::stop();
            }
        }
    }
}

/// The message for a send refused because this chat already has a turn.
const TURN_IN_PROGRESS: &str =
    "This chat is still finishing its previous reply. Wait a moment, then send again.";

/// Marks a session in flight for as long as it lives. Only the task that owns
/// the turn holds one, so a refused second send can never clear the flag of
/// the turn that is still running.
struct InFlight {
    app: AppHandle,
    session_id: String,
}

impl InFlight {
    /// `None` when this session already has a turn in flight from Kitty.
    fn claim(app: &AppHandle, session_id: &str) -> Option<Self> {
        let state = app.state::<AppState>();
        let inserted = state
            .in_flight_sessions
            .lock()
            .unwrap()
            .insert(session_id.to_string());
        inserted.then(|| Self {
            app: app.clone(),
            session_id: session_id.to_string(),
        })
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.app
            .state::<AppState>()
            .in_flight_sessions
            .lock()
            .unwrap()
            .remove(&self.session_id);
        notifications::refresh_tray(&self.app);
    }
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
    // Claimed before anything else, and answered to the calling window only:
    // a refused second send must neither touch the running turn's flag nor
    // broadcast an error that would end that turn in every window.
    let in_flight = InFlight::claim(&app, &session_id).ok_or(TURN_IN_PROGRESS)?;
    notifications::refresh_tray(&app);

    // Register any drag-and-drop / pasted file paths as this session's
    // approval-free read set BEFORE the turn streams, so the model can open an
    // attachment directly instead of tripping a sandbox HITL approval every time
    // (the daemon unions these into `metadata.attached_paths`, which
    // `sandbox::allowed_dirs_for_session` honours). Awaited before the SSE call
    // so the allowance is in place when the tool loop computes allowed_dirs.
    // Directories and files are registered under different keys, because they
    // are different grants. An attached *file* allows exactly that file
    // (`path_within_any`'s equality case); a *directory* widens to its whole
    // subtree for the rest of the session. Filing a dropped folder under
    // `attached_paths` made those indistinguishable in the UI — a subtree grant
    // showed up looking like "a file the user handed over" — so a folder goes
    // to `working_dirs` instead, which is where the pill's hover list reads
    // from and where it can be revoked.
    let (dirs, files): (Vec<String>, Vec<String>) = attached_paths
        .unwrap_or_default()
        .into_iter()
        .filter(|p| !p.trim().is_empty())
        .partition(|p| std::path::Path::new(p).is_dir());

    if !dirs.is_empty() || !files.is_empty() {
        let mut patch = serde_json::Map::new();
        if !files.is_empty() {
            patch.insert("attached_paths".into(), json!(files));
        }
        if !dirs.is_empty() {
            patch.insert("working_dirs".into(), json!(dirs));
        }
        if let Err(e) = client
            .patch_json(
                &format!("/api/chat/{session_id}/config"),
                &Value::Object(patch),
            )
            .await
        {
            // Non-fatal: the path is still named in the prompt text, so at
            // worst the model hits the old approval flow for it.
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

    // Keep the process foreground for the whole turn on Android; moved into
    // the task below and dropped at its end. No-op on desktop.
    let turn_foreground = foreground::TurnSession::start();
    // Open the stream here, not in the task: a refusal (409: a turn started
    // elsewhere, e.g. by a schedule) is this caller's error alone.
    let resp = open_send_stream(&client, &session_id, &body)
        .await
        .map_err(|f| f.message)?;
    // The card this chat is pinned to: its Response timeout bounds the
    // stream, and its reachability is what the result reports on. Known
    // this run in almost every case; the engine is asked only on a miss.
    let card = session_card(&app, &client, &session_id).await;

    let app_bg = app.clone();
    tauri::async_runtime::spawn(async move {
        let _turn_foreground = turn_foreground;
        let _in_flight = in_flight;
        let outcome = consume_stream(&app_bg, &session_id, resp, idle_for(card.as_ref())).await;
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
                // A turn the user stopped did not "finish".
                if !cancelled {
                    notifications::notify_if_hidden(
                        &app_bg,
                        notifications::Event::TaskComplete,
                        "Kitty finished",
                        "Your task is complete.",
                        Some(&session_id),
                    );
                }
                providers::emit_health_from_send_result(&app_bg, card.as_ref(), None);
                poll_compaction_status(app_bg.clone(), session_id.clone());
                poll_session_title(app_bg.clone(), session_id.clone());
            }
            Ok(TurnOutcome {
                error: Some(message),
                error_type,
                ..
            }) => {
                if error_type.as_deref() == Some("effort_rejected") {
                    crate::bigtiny::effort::on_effort_rejected(&app_bg, &session_id);
                }
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
                providers::emit_health_from_send_result(
                    &app_bg,
                    card.as_ref(),
                    Some(error_type.as_deref().unwrap_or("")),
                );
                // The engine starts a forced compaction after an overflow;
                // watch for it so the chat reopens when it lands.
                if error_type.as_deref() == Some("context_exceeded") {
                    poll_forced_compaction(app_bg.clone(), session_id.clone());
                }
            }
            Err(StreamFailure {
                message,
                error_type,
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
            }
        }
        drop(_in_flight);
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
        check_compaction(&app, &session_id).await;
    });
}

/// After a `context_exceeded` turn: the engine's forced pass takes as long as
/// the summarizer does, so look several times. The first look is immediate,
/// so it records the watermark from before the pass as the baseline.
fn poll_forced_compaction(app: AppHandle, session_id: String) {
    tauri::async_runtime::spawn(async move {
        for delay in [0u64, 4, 8, 15, 30, 60, 120] {
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            if check_compaction(&app, &session_id).await {
                return;
            }
        }
    });
}

/// Read the chat's compaction watermark and announce it if it moved since
/// the last look. The first look at a chat in this run only records a
/// baseline: a missing entry is "unknown", not 0, or every previously
/// compacted chat would announce an old compaction once per run. `true` when
/// a compaction was announced.
async fn check_compaction(app: &AppHandle, session_id: &str) -> bool {
    let Ok(stats) = crate::bigtiny::sessions::get_stats(app, session_id).await else {
        return false;
    };
    let Some(rowid) = stats
        .get("compacted_through_rowid")
        .and_then(|v| v.as_i64())
    else {
        return false;
    };
    let previous = {
        let state = app.state::<AppState>();
        let mut watermarks = state.bigtiny_compaction_watermarks.lock().unwrap();
        watermarks.insert(session_id.to_string(), rowid)
    };
    match previous {
        Some(previous) if rowid > previous => {
            let _ = app.emit(
                "chat://compaction",
                json!({
                    "session_id": session_id,
                    "compacted_through_rowid": rowid,
                    "memory_slots": stats.get("memory_slots"),
                }),
            );
            true
        }
        _ => false,
    }
}

/// BigTiny derives a session's title in a detached task that runs *after*
/// the turn completes — and therefore after the turn's terminal (`is_last`)
/// SSE frame, which is the frame `run_stream` stops reading at. Its
/// `session_title` event lands on a connection nobody is reading any more,
/// which is why an auto-titled chat used to stay "New Chat" in the sidebar
/// until something else made the list refresh.
///
/// So: poll for it, the same way `poll_compaction_status` above covers the
/// same gap for compaction. Emits the identical `chat://session-title` event
/// the stream path emits, so the frontend needs no second code path.
///
/// Only the *first* completed turn of a session can produce a title (BigTiny
/// never re-derives one once set), so a session is polled at most once per
/// app run: `bigtiny_titled_sessions` records the ones already resolved,
/// including those a user renamed by hand.
fn poll_session_title(app: AppHandle, session_id: String) {
    if app
        .state::<AppState>()
        .bigtiny_titled_sessions
        .lock()
        .unwrap()
        .contains(&session_id)
    {
        return;
    }
    tauri::async_runtime::spawn(async move {
        // The title is one summarizer call, but it queues behind the turn's
        // own teardown and (on a cold local model) a load. Retry rather than
        // guess one delay: a title that lands late is still worth showing,
        // and giving up after ~20s costs nothing — the next app start reads
        // the persisted name anyway.
        for delay in [2u64, 3, 5, 10] {
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            // An engine that publishes the title on the app event stream has
            // already delivered it (`lifecycle::app_events`); this poll only
            // covers an older engine.
            if app
                .state::<AppState>()
                .bigtiny_titled_sessions
                .lock()
                .unwrap()
                .contains(&session_id)
            {
                return;
            }

            let Ok(sessions) = crate::bigtiny::sessions::list(&app).await else {
                continue;
            };
            let Some(row) = sessions
                .iter()
                .find(|s| s.get("sessionId").and_then(|v| v.as_str()) == Some(session_id.as_str()))
            else {
                // Deleted mid-poll, or older than the list window. Either
                // way there is nothing left to title.
                return;
            };
            let title = row.get("title").and_then(|v| v.as_str()).unwrap_or("");
            // `translate_session_row` substitutes "New Chat" for a session
            // BigTiny hasn't named yet, so that is the "still waiting"
            // sentinel, not a real title.
            if title.is_empty() || title == "New Chat" {
                continue;
            }
            app.state::<AppState>()
                .bigtiny_titled_sessions
                .lock()
                .unwrap()
                .insert(session_id.clone());
            let _ = app.emit(
                "chat://session-title",
                json!({ "session_id": session_id, "title": title }),
            );
            return;
        }
    });
}

/// The card this session is pinned to - not the default card, which may be a
/// different provider entirely.
async fn session_card(
    app: &AppHandle,
    client: &BigTinyClient,
    session_id: &str,
) -> Option<crate::config::providers::ProviderProfile> {
    // Which card each chat is on is remembered when a chat is created,
    // loaded, branched or moved; reading the engine's metadata (which pages
    // the session list) is only for a chat this run has not seen.
    let known = {
        let state = app.state::<AppState>();
        let c = state
            .session_cards
            .lock()
            .unwrap()
            .get(session_id)
            .map(|(card, _)| card.clone());
        c
    };
    let provider = match known {
        Some(card) => card,
        None => {
            let meta = crate::bigtiny::sessions::metadata(client, session_id).await;
            meta.get("provider").and_then(|p| p.as_str())?.to_string()
        }
    };
    let state = app.state::<AppState>();
    let cfg = state.config.lock().unwrap();
    cfg.providers.iter().find(|p| p.id == provider).cloned()
}

/// The chat's own card's `prompt_idle_timeout_secs`, or 300s when unset.
fn idle_for(card: Option<&crate::config::providers::ProviderProfile>) -> std::time::Duration {
    std::time::Duration::from_secs(u64::from(
        card.and_then(|c| c.prompt_idle_timeout_secs)
            .filter(|s| *s > 0)
            .unwrap_or(300),
    ))
}

/// Start a turn: POST `/send` and return the open SSE response, or the
/// refusal.
async fn open_send_stream(
    client: &BigTinyClient,
    session_id: &str,
    body: &Value,
) -> Result<reqwest::Response, StreamFailure> {
    let resp = client
        .request_stream(
            reqwest::Method::POST,
            &format!("/api/chat/{session_id}/send"),
        )
        .json(body)
        .send()
        .await
        .map_err(|e| format!("Could not reach the engine to send the message: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(send_refusal(status, &text));
    }
    Ok(resp)
}

/// Read one turn's SSE stream to its end, emitting `chat://*` events as frames
/// arrive: the reading half of [`run_stream`], shared with
/// [`attach_session_stream`].
async fn consume_stream(
    app: &AppHandle,
    session_id: &str,
    resp: reqwest::Response,
    idle: std::time::Duration,
) -> Result<TurnOutcome, StreamFailure> {
    let mut outcome = TurnOutcome::default();
    // Tool cards are paired by the daemon's own `tool_call_id`, because a
    // step's tool calls run CONCURRENTLY (`agent::loop_::execute_tools`) —
    // several `tool_start` frames arrive before any `tool_finish`, so arrival
    // order says nothing about which finish belongs to which start. The single
    // "current call" slot this replaced attributed each finish to whichever
    // card started last, which left the longest call in a batch — an
    // `await_specialists` alongside the `call_specialist`s that opened it —
    // showing "Waiting for specialists" forever, long after the daemon had
    // collected the reports and moved on.
    //
    // `tool_seq` survives as the fallback for frames with no `tool_call_id`: a
    // daemon binary older than this field, and `server::replay.rs`, still emit
    // them, and one unpaired card beats no card at all.
    let mut tool_seq: u64 = 0;
    let mut open_tools: HashMap<String, String> = HashMap::new();
    let mut last_tool: Option<String> = None;

    // Raw-byte accumulation buffer: frames are decoded only once complete
    // (`drain_complete_frames`), so a multi-byte UTF-8 character split
    // across two TCP chunks survives intact.
    let mut buffer: Vec<u8> = Vec::new();
    let mut scan_from: usize = 0;
    // Idle-only deadline on the stream: if the daemon sends no bytes for this
    // long, the turn is wedged and we bail out (see the `Elapsed` arm). The
    // daemon sends a keepalive comment every 15s, and those are bytes too, so
    // a turn that is merely waiting - on an approval, on a slow model - is
    // never cut off; this only fires when the daemon itself stops answering.
    let mut bytes = resp.bytes_stream();
    let mut deltas = DeltaBatcher::default();
    let mut saw_last = false;
    'outer: loop {
        let chunk = match tokio::time::timeout(idle, bytes.next()).await {
            Ok(Some(item)) => {
                item.map_err(|e| format!("The connection to the engine broke: {e}"))?
            }
            Ok(None) => break, // daemon closed the stream
            Err(_) => {
                return Err(StreamFailure {
                    message: format!(
                        "The engine stopped responding for {}s, so this reply was abandoned. \
                         Send it again; if it keeps happening, restart the engine.",
                        idle.as_secs()
                    ),
                    error_type: Some("idle_timeout"),
                });
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
                &mut open_tools,
                &mut last_tool,
                &mut deltas,
            );
            if is_last {
                deltas.flush(app, session_id);
                saw_last = true;
                break 'outer;
            }
        }
        // Time-based release of whatever text accumulated across this chunk's
        // frames. Checked per chunk rather than per frame so a burst of tokens
        // in one chunk still goes out as a single event.
        if deltas.due() {
            deltas.flush(app, session_id);
        }
    }
    // The stream can also end via `Ok(None)` (daemon closed cleanly) without an
    // `is_last` frame, so flush once more rather than stranding buffered text.
    deltas.flush(app, session_id);
    // A stream that ends without its terminal frame did not finish: the
    // engine went away mid-turn (shutdown, crash). Reporting it as a
    // completed reply would show "Kitty finished" for a half answer.
    if !saw_last && !outcome.cancelled && outcome.error.is_none() {
        return Err(StreamFailure {
            message: "The engine closed the connection before the reply finished. \
                      Send it again; if it keeps happening, restart the engine."
                .to_string(),
            error_type: Some("stream_closed"),
        });
    }
    Ok(outcome)
}

/// How long streamed text may accumulate before it is emitted to the webview.
///
/// BigTiny emits one SSE frame per token, and this used to forward each one as
/// its own Tauri event — a JSON serialization plus an IPC hop across the
/// webview bridge, per token, per open window. The frontend has always
/// coalesced deltas into one store write per animation frame
/// (`chatStore.ts`'s `bufferDelta`/`flushDeltas`), so batching here changes
/// nothing it can observe: at 25 ms the webview still receives text faster
/// than it can paint it.
const DELTA_BATCH_MS: u128 = 25;

/// Accumulates streamed text so it can be emitted in batches rather than per
/// token.
///
/// Ordering is the thing to preserve: a tool call, an error, or the terminal
/// `complete` event must never overtake the text that preceded it. Every
/// non-delta emission is therefore preceded by a `flush`, and the stream
/// flushes once more when it ends.
#[derive(Default)]
struct DeltaBatcher {
    text: String,
    reasoning: String,
    last_flush: Option<std::time::Instant>,
}

impl DeltaBatcher {
    fn push_text(&mut self, s: &str) {
        self.text.push_str(s);
    }

    fn push_reasoning(&mut self, s: &str) {
        self.reasoning.push_str(s);
    }

    /// True once enough time has passed that the buffered text should go out.
    fn due(&self) -> bool {
        if self.text.is_empty() && self.reasoning.is_empty() {
            return false;
        }
        match self.last_flush {
            None => true,
            Some(t) => t.elapsed().as_millis() >= DELTA_BATCH_MS,
        }
    }

    fn flush(&mut self, app: &AppHandle, session_id: &str) {
        if !self.text.is_empty() {
            let _ = app.emit(
                "chat://message-delta",
                json!({ "session_id": session_id, "text": self.text }),
            );
            self.text.clear();
        }
        if !self.reasoning.is_empty() {
            let _ = app.emit(
                "chat://reasoning-delta",
                json!({ "session_id": session_id, "text": self.reasoning }),
            );
            self.reasoning.clear();
        }
        self.last_flush = Some(std::time::Instant::now());
    }
}

/// Translate one BigTiny SSE event into its `chat://*` emission(s).
#[allow(clippy::too_many_arguments)]
fn handle_event(
    app: &AppHandle,
    session_id: &str,
    event: &Value,
    outcome: &mut TurnOutcome,
    tool_seq: &mut u64,
    // `open_tools`: the daemon's `tool_call_id` -> the card id handed to the
    // webview, for every call started and not yet finished. `last_tool`: the
    // most recent card id, used only to pair a frame carrying no
    // `tool_call_id` (see the declarations in `consume_stream`).
    open_tools: &mut HashMap<String, String>,
    last_tool: &mut Option<String>,
    deltas: &mut DeltaBatcher,
) {
    let kind = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let content = event.get("content").and_then(|c| c.as_str());
    let tool_name = event
        .get("tool_name")
        .and_then(|t| t.as_str())
        .unwrap_or("");

    // Text accumulates in the batcher; everything else flushes it first so a
    // later event can never be delivered ahead of the text it followed.
    if kind != "llm_delta" && kind != "reasoning_delta" {
        deltas.flush(app, session_id);
    }

    match kind {
        "llm_delta" => {
            if let Some(text) = content {
                deltas.push_text(text);
            }
        }
        "reasoning_delta" => {
            if let Some(text) = content {
                deltas.push_reasoning(text);
            }
        }
        "tool_start" => {
            let id = open_tool_card(
                event.get("tool_call_id").and_then(|v| v.as_str()),
                tool_seq,
                open_tools,
                last_tool,
            );
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
            // The step budget ran out: not a tool the user watched start, but
            // the reason this turn stopped where it did, which they are owed.
            if tool_name == "__budget__" {
                let _ = app.emit(
                    "chat://notice",
                    json!({
                        "session_id": session_id,
                        "kind": "step_limit",
                        "message": event.get("tool_result").and_then(|r| r.as_str()).unwrap_or(""),
                    }),
                );
                return;
            }
            // The wrap-up valve's notice, which travels on the same synthetic
            // channel but is emphatically NOT internal bookkeeping: the turn
            // just ended early and the answer is shorter than it would have
            // been, so the user is owed the reason. Routed to its own event
            // (a dismissible banner, like the compaction notice) rather than
            // rendered as a tool card for a tool nobody called.
            if tool_name == "__context_budget__" {
                let _ = app.emit(
                    "chat://context-budget",
                    json!({
                        "session_id": session_id,
                        "message": event.get("tool_result").and_then(|r| r.as_str()).unwrap_or(""),
                    }),
                );
                return;
            }
            let call_id = event.get("tool_call_id").and_then(|v| v.as_str());
            let id = close_tool_card(call_id, *tool_seq, open_tools, last_tool);
            let result_text = event
                .get("tool_result")
                .and_then(|r| r.as_str())
                .unwrap_or("");
            let failed = event
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or_else(|| tool_result_is_error(result_text));
            let _ = app.emit(
                "chat://tool-call",
                json!({
                    "session_id": session_id,
                    "phase": "tool_call_update",
                    "update": tool_update(&id, call_id, failed, result_text),
                }),
            );
            // End-of-turn outcome recording now lives in the BigTiny daemon
            // (`bigtiny2::agent::loop_::spawn_record_outcome`) where the
            // real context + reward source is available; the old app-layer
            // context-free backstop was removed to avoid double-recording the
            // same tool outcome to AP (which would skew learning rewards).
        }
        "subagent_status" => {
            // The `session_id` *on the frame* is the delegate's, not the
            // parent's, so the two swap here: this handler's `session_id`
            // argument is the stream being read, i.e. the parent. That is
            // deliberate daemon-side — the child id is what lets the UI offer a
            // click-through into the delegate's own transcript, the escape
            // hatch for when its structured report was not enough.
            let _ = app.emit(
                "chat://subagent-status",
                json!({
                    "session_id": session_id,
                    "child_session_id": event.get("session_id"),
                    "specialist": event.get("tool_name"),
                    "status": content,
                    "error": event.get("error_message"),
                    // What the delegate is actually running on. A delegate is
                    // routinely hosted on a different provider AND model from
                    // the session that started it (`subagent_pick::choose_host`),
                    // so without these a client has nothing to label it with but
                    // the parent's own provider — which is the wrong answer,
                    // every time it differs.
                    "provider_id": event.get("provider_id"),
                    "model": event.get("model"),
                }),
            );
        }
        "session_title" => {
            if let Some(title) = content {
                // Reaches us only when the daemon happens to emit it before
                // the terminal frame; `poll_session_title` covers the usual
                // case where it doesn't. Recording it here stops that poll
                // from running for a session already titled.
                app.state::<AppState>()
                    .bigtiny_titled_sessions
                    .lock()
                    .unwrap()
                    .insert(session_id.to_string());
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
        "model_failover" => {
            // The model this turn runs on changed: the pinned provider was
            // unavailable, cannot call tools, or failed mid-turn. Said out
            // loud rather than silently, and the chat's model info follows.
            let text = |k: &str| event.get(k).and_then(|v| v.as_str());
            let _ = app.emit(
                "chat://notice",
                json!({
                    "session_id": session_id,
                    "kind": "failover",
                    "reason": text("reason"),
                    "provider_id": text("provider_id"),
                    "from_provider_id": text("from_provider_id"),
                    "model": text("model"),
                    "message": content,
                }),
            );
        }
        _ => {}
    }
}

/// Open a tool card, returning the id the webview will know it by.
///
/// Keyed by the daemon's own `tool_call_id` when the frame carries one. A
/// step's tool calls run concurrently, so several cards are open at once and
/// arrival order cannot pair a later finish with its start.
fn open_tool_card(
    call_id: Option<&str>,
    tool_seq: &mut u64,
    open_tools: &mut HashMap<String, String>,
    last_tool: &mut Option<String>,
) -> String {
    *tool_seq += 1;
    let id = format!("bt-{tool_seq}");
    match call_id {
        Some(call_id) => {
            open_tools.insert(call_id.to_string(), id.clone());
        }
        // Pre-`tool_call_id` daemon, or a replayed frame: fall back to the old
        // single-slot behaviour, which is correct for the sequential case it
        // was written for.
        None => *last_tool = Some(id.clone()),
    }
    id
}

/// The card id a `tool_finish` belongs to, closing it.
///
/// An unrecognised `tool_call_id` falls through to the sequential fallback
/// rather than inventing a card: a finish arriving for a start we never saw is
/// better rendered against the newest card than dropped.
fn close_tool_card(
    call_id: Option<&str>,
    tool_seq: u64,
    open_tools: &mut HashMap<String, String>,
    last_tool: &mut Option<String>,
) -> String {
    call_id
        .and_then(|call_id| open_tools.remove(call_id))
        .or_else(|| last_tool.take())
        .unwrap_or_else(|| format!("bt-{tool_seq}"))
}

/// Whether a tool result reads as a failure, for a frame without the daemon's
/// `is_error` (a replayed message; a daemon older than the flag). Kitty's own
/// tools answer failures with a JSON envelope (`"status": "error"`), which a
/// text-prefix check alone read as success.
pub(crate) fn tool_result_is_error(result_text: &str) -> bool {
    let trimmed = result_text.trim_start();
    if trimmed.starts_with("Error") || trimmed.starts_with("[Tool error") {
        return true;
    }
    if trimmed.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            return v.get("status").and_then(|s| s.as_str()) == Some("error");
        }
    }
    false
}

/// The `chat://tool-call` update for a finished tool. `daemonToolCallId` is
/// what `fetch_full_tool_result` looks the full output up by when
/// `truncated` says the card got less than all of it.
pub(crate) fn tool_update(
    card_id: &str,
    call_id: Option<&str>,
    failed: bool,
    output: &str,
) -> Value {
    let shown = truncate_for_ui(output);
    json!({
        "toolCallId": card_id,
        "daemonToolCallId": call_id,
        "status": if failed { "failed" } else { "completed" },
        "truncated": shown != output,
        "rawOutput": shown,
    })
}

/// The whole output of one tool call, from the chat's history: what a card
/// showing a truncated result fetches on "Show full output".
pub async fn fetch_full_tool_result(
    app: &AppHandle,
    session_id: &str,
    tool_call_id: &str,
) -> Result<String, String> {
    let client = ensure_client(app)?;
    // Asks for that one row (`tool_call_id`); an engine that does not know
    // the filter ignores it and sends the whole history, which the search
    // below handles the same way.
    let history = client
        .get_json_long(&format!(
            "/api/chat/{session_id}/history?limit=10000&tool_call_id={}",
            urlencoding_component(tool_call_id)
        ))
        .await?;
    let rows = history
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .or_else(|| history.as_array().cloned())
        .unwrap_or_default();
    rows.iter()
        .find(|r| {
            r.get("role").and_then(|v| v.as_str()) == Some("tool")
                && r.get("tool_call_id").and_then(|v| v.as_str()) == Some(tool_call_id)
        })
        .and_then(|r| {
            r.get("content")
                .and_then(|c| c.as_str())
                .map(str::to_string)
        })
        .ok_or_else(|| "That tool's output is no longer in this chat's history.".to_string())
}

/// Percent-encode a query value (ids are normally plain, but nothing
/// guarantees it).
fn urlencoding_component(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Follow a turn already running in another place (a specialist's own chat,
/// opened to watch it): its frames so far, then live, as `chat://*` events,
/// ending with `chat://complete`. `Ok(false)` when nothing is running there.
pub async fn attach_session_stream(app: &AppHandle, session_id: &str) -> Result<bool, String> {
    let client = ensure_client(app)?;
    let resp = client
        .request_stream(
            reqwest::Method::GET,
            &format!("/api/chat/{session_id}/stream"),
        )
        .send()
        .await
        .map_err(|e| format!("Could not reach the engine: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(false);
    }
    if !resp.status().is_success() {
        return Err(format!("The engine answered {}", resp.status()));
    }
    let app = app.clone();
    let session_id = session_id.to_string();
    tauri::async_runtime::spawn(async move {
        let outcome =
            consume_stream(&app, &session_id, resp, std::time::Duration::from_secs(300)).await;
        match outcome {
            Ok(o) if o.error.is_none() => {
                let _ = app.emit(
                    "chat://complete",
                    json!({ "session_id": session_id, "result": { "stopReason": if o.cancelled { "cancelled" } else { "end_turn" } } }),
                );
            }
            Ok(o) => {
                let _ = app.emit(
                    "chat://error",
                    json!({ "session_id": session_id, "message": o.error, "error_type": o.error_type }),
                );
            }
            Err(f) => {
                let _ = app.emit(
                    "chat://error",
                    json!({ "session_id": session_id, "message": f.message, "error_type": f.error_type }),
                );
            }
        }
    });
    Ok(true)
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

/// Answer a tool approval waiting on a person. `decision` is the daemon's:
/// `allow`, `always_allow` or `reject`. An `always_allow` is stored with the
/// approval's scope (`approvals::always_scope`): the command's first words
/// for a shell command, the whole tool otherwise.
pub async fn answer_approval(
    app: &AppHandle,
    action_id: &str,
    decision: &str,
) -> Result<(), String> {
    // Read, not removed: if the POST fails the approval is still pending and
    // can be answered again. The daemon's `hitl_resolved` removes it.
    let pending = app
        .state::<AppState>()
        .pending_approvals
        .lock()
        .unwrap()
        .get(action_id)
        .cloned()
        .ok_or("That approval request is no longer pending.")?;
    let pattern = (decision == "always_allow")
        .then_some(pending.always_scope.args_pattern.as_deref())
        .flatten();
    crate::lifecycle::app_events::approve(app, &pending.session_id, action_id, decision, pattern)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The failure this pairing replaced: a step's tool calls run concurrently
    /// (`agent::loop_::execute_tools`), so all three starts arrive before any
    /// finish. Paired by arrival order, the first finish claimed the third
    /// card and the longest call — the `await_specialists` opened by the two
    /// `call_specialist`s beside it — was never completed at all.
    #[test]
    fn concurrent_tool_calls_pair_by_call_id() {
        let mut seq = 0u64;
        let mut open = HashMap::new();
        let mut last = None;

        let a = open_tool_card(Some("call-a"), &mut seq, &mut open, &mut last);
        let b = open_tool_card(Some("call-b"), &mut seq, &mut open, &mut last);
        let c = open_tool_card(Some("call-c"), &mut seq, &mut open, &mut last);
        assert_eq!(
            (a.as_str(), b.as_str(), c.as_str()),
            ("bt-1", "bt-2", "bt-3")
        );

        // Finishing out of order — the usual case, and the whole point.
        assert_eq!(
            close_tool_card(Some("call-b"), seq, &mut open, &mut last),
            b
        );
        assert_eq!(
            close_tool_card(Some("call-a"), seq, &mut open, &mut last),
            a
        );
        assert_eq!(
            close_tool_card(Some("call-c"), seq, &mut open, &mut last),
            c
        );
        assert!(open.is_empty(), "every card closed exactly once");
    }

    /// A daemon older than `tool_call_id` (and `server::replay.rs`) emits
    /// neither field; the sequential fallback must still render a paired card.
    #[test]
    fn frames_without_a_call_id_fall_back_to_sequence() {
        let mut seq = 0u64;
        let mut open = HashMap::new();
        let mut last = None;

        let a = open_tool_card(None, &mut seq, &mut open, &mut last);
        assert_eq!(close_tool_card(None, seq, &mut open, &mut last), a);
        let b = open_tool_card(None, &mut seq, &mut open, &mut last);
        assert_eq!(close_tool_card(None, seq, &mut open, &mut last), b);
        assert_ne!(a, b);
    }

    /// A finish for a start we never saw is rendered against the newest card
    /// rather than dropped.
    #[test]
    fn an_unknown_call_id_still_closes_a_card() {
        let mut seq = 0u64;
        let mut open = HashMap::new();
        let mut last = None;

        let a = open_tool_card(Some("call-a"), &mut seq, &mut open, &mut last);
        assert_eq!(
            close_tool_card(Some("nope"), seq, &mut open, &mut last),
            "bt-1"
        );
        // The real card is still open, so its own finish still finds it.
        assert_eq!(
            close_tool_card(Some("call-a"), seq, &mut open, &mut last),
            a
        );
    }

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

    /// Kitty's tools report failure in a JSON envelope; that must read as a
    /// failure too (#34), and ordinary JSON must not.
    #[test]
    fn a_json_error_envelope_is_a_failed_tool() {
        assert!(tool_result_is_error(
            "{\n  \"status\": \"error\",\n  \"error_code\": \"PATH_OUTSIDE_HOME\"\n}"
        ));
        assert!(!tool_result_is_error(
            "{\"status\": \"success\", \"data\": 1}"
        ));
        assert!(!tool_result_is_error("{not json"));
    }

    #[test]
    fn a_truncated_output_is_marked_and_keeps_its_call_id() {
        let long = "x".repeat(MAX_STRING_BYTES + 10);
        let u = tool_update("bt-1", Some("call-9"), false, &long);
        assert_eq!(u["truncated"], true);
        assert_eq!(u["daemonToolCallId"], "call-9");
        assert_eq!(tool_update("bt-1", None, true, "short")["truncated"], false);
    }

    #[test]
    fn a_409_reads_as_a_turn_in_progress() {
        let f = send_refusal(reqwest::StatusCode::CONFLICT, "busy");
        assert_eq!(f.error_type, Some("turn_in_progress"));
        assert!(!f.message.contains("409"));
        assert_eq!(
            send_refusal(reqwest::StatusCode::BAD_GATEWAY, "x").error_type,
            None
        );
    }

    #[test]
    fn a_text_error_prefix_is_a_failed_tool() {
        assert!(!tool_result_is_error("file contents here"));
        assert!(!tool_result_is_error(""));
        assert!(tool_result_is_error("Error: file not found"));
        assert!(tool_result_is_error("[Tool error: timeout]"));
    }
}
