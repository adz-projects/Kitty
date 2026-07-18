//! ACP JSON-RPC client over a single bidirectional WebSocket.
//!
//! One connection multiplexes all sessions. Outgoing requests are matched to
//! responses by id via a pending-oneshot map; incoming notifications and
//! server-initiated requests are dispatched in [`crate::goosed::stream`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::goosed::stream;
use crate::state::AppState;

/// Default idle-reset window for `request_session_prompt` — used when the
/// active provider has no `prompt_idle_timeout_secs` override configured.
pub const DEFAULT_PROMPT_IDLE_SECS: u64 = 300;

pub use crate::goosed::types::{Activity, Pending, Perm, ToolCalls};

/// Outbound-message channel capacity. Bounded (rather than unbounded) so a
/// stalled goosed WebSocket applies backpressure to senders instead of
/// letting queued messages grow without limit — see MINOR_BUGS.md #5.
/// Generous relative to real concurrency (one writer per open session,
/// plus rare notify/respond calls), so a healthy connection never blocks.
const ACP_OUT_CHANNEL_CAPACITY: usize = 64;

/// Cheap-to-clone handle to the live ACP connection (holds channel + shared maps).
#[derive(Clone)]
pub struct AcpClient {
    out: mpsc::Sender<Message>,
    pending: Pending,
    permissions: Perm,
    activity: Activity,
    next_id: Arc<AtomicI64>,
}

impl AcpClient {
    /// Connect, spawn reader/writer tasks, and complete the `initialize`
    /// handshake. On disconnect the reader clears `AppState.acp` so the next
    /// call reconnects.
    pub async fn connect(app: AppHandle, port: u16, secret: &str) -> Result<AcpClient, String> {
        let url = format!("ws://127.0.0.1:{port}/acp?token={secret}");
        let mut req = url
            .into_client_request()
            .map_err(|e| format!("bad ACP url: {e}"))?;
        let hv = secret
            .parse()
            .map_err(|_| "invalid secret header".to_string())?;
        req.headers_mut().insert("X-Secret-Key", hv);

        let (ws, _resp) = tokio_tungstenite::connect_async(req)
            .await
            .map_err(|e| format!("ACP connect failed: {e}"))?;
        let (mut write, mut read) = ws.split();

        let (out_tx, mut out_rx) = mpsc::channel::<Message>(ACP_OUT_CHANNEL_CAPACITY);
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if write.send(msg).await.is_err() {
                    break;
                }
            }
        });

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let permissions: Perm = Arc::new(Mutex::new(HashMap::new()));
        let activity: Activity = Arc::new(Mutex::new(HashMap::new()));
        let tool_calls: ToolCalls = Arc::new(Mutex::new(HashMap::new()));
        let client = AcpClient {
            out: out_tx.clone(),
            pending: pending.clone(),
            permissions: permissions.clone(),
            activity: activity.clone(),
            next_id: Arc::new(AtomicI64::new(0)),
        };

        let app_reader = app.clone();
        let out_reader = out_tx.clone();
        tokio::spawn(async move {
            while let Some(next) = read.next().await {
                match next {
                    Ok(Message::Text(txt)) => {
                        stream::handle_incoming(
                            &app_reader,
                            &out_reader,
                            &pending,
                            &permissions,
                            &activity,
                            &tool_calls,
                            &txt,
                        )
                        .await;
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
            tracing::warn!("ACP websocket closed");
            // Fail any in-flight requests fast rather than letting each block for
            // the full 300s timeout — the connection is gone, no response is coming.
            for (_, tx) in pending.lock().await.drain() {
                let _ = tx.send(Err(json!({ "message": "ACP connection closed" })));
            }
            // Allow reconnect on the next command.
            *app_reader.state::<AppState>().acp.lock().await = None;
        });

        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false } }
                }),
            )
            .await?;

        Ok(client)
    }

    /// Send a JSON-RPC request and await its response (300s cap covers long turns).
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.out
            .send(Message::Text(msg.to_string()))
            .await
            .map_err(|_| "ACP connection closed".to_string())?;

        match tokio::time::timeout(Duration::from_secs(300), rx).await {
            Ok(Ok(Ok(val))) => Ok(val),
            Ok(Ok(Err(err))) => Err(acp_error_message(&err)),
            Ok(Err(_)) => Err("ACP request cancelled".into()),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err("ACP request timed out".into())
            }
        }
    }

    /// `session/prompt`-specific send: unlike `request()`'s single flat
    /// timeout, the deadline slides forward every time a `session/update`
    /// notification arrives for this session (tracked in `self.activity` by
    /// `stream::handle_incoming`) — a turn that's actively streaming tokens or
    /// tool calls over a slow connection (e.g. Tailscale) is never killed just
    /// for taking a while, only for genuine silence. `idle_secs` (per-provider
    /// configurable — see `ProviderProfile::prompt_idle_timeout_secs`, defaults
    /// to `DEFAULT_PROMPT_IDLE_SECS` when the caller passes that constant) is
    /// the same window `request()`'s flat cap uses by default, so a
    /// cold/slow-starting turn (before any notification has arrived) is
    /// tolerated exactly as long as today at the default setting — the only
    /// behavior change is that the clock resets once streaming begins.
    /// `PROMPT_ABSOLUTE_CEILING_SECS` is a generous backstop against a
    /// connection that's technically alive (stray notifications) but never
    /// actually finishing; the user already has a manual escape hatch for "I
    /// want to bail now" (Force Stop), so this can afford to be generous
    /// rather than cut off a legitimate long agentic turn.
    pub async fn request_session_prompt(
        &self,
        session_id: &str,
        params: Value,
        idle_secs: u64,
    ) -> Result<Value, String> {
        const PROMPT_ABSOLUTE_CEILING_SECS: u64 = 7200;

        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, mut rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let msg =
            json!({ "jsonrpc": "2.0", "id": id, "method": "session/prompt", "params": params });
        self.out
            .send(Message::Text(msg.to_string()))
            .await
            .map_err(|_| "ACP connection closed".to_string())?;

        let start = Instant::now();
        let idle = Duration::from_secs(idle_secs);
        let ceiling = Duration::from_secs(PROMPT_ABSOLUTE_CEILING_SECS);
        let absolute_deadline = start + ceiling;

        // Reset this session's activity clock to *now*, not whenever the
        // previous turn last streamed something. Without this, a user who
        // takes their time before replying (there is no timeout on that —
        // nothing is in flight while they're composing) would have their
        // *next* send immediately judged against a stale timestamp from the
        // prior turn: if they waited longer than `idle_secs` between messages,
        // `last_activity + idle` would already be in the past the instant this
        // new request starts, timing it out on the very first loop check
        // regardless of whether the model would have answered fine. Confirmed
        // root cause of "if I take too long to respond, my next message times
        // out" — the user's own idle time must never count against the
        // *response* they haven't asked for yet.
        self.activity
            .lock()
            .await
            .insert(session_id.to_string(), start);

        loop {
            let last_activity = self
                .activity
                .lock()
                .await
                .get(session_id)
                .copied()
                .unwrap_or(start);
            let idle_deadline = last_activity + idle;
            let deadline = idle_deadline.min(absolute_deadline);
            let now = Instant::now();
            if now >= deadline {
                self.pending.lock().await.remove(&id);
                return Err(if idle_deadline <= absolute_deadline {
                    format!(
                        "ACP request timed out (no response for {} minutes)",
                        idle_secs / 60
                    )
                } else {
                    "ACP request timed out (exceeded the 2-hour safety ceiling)".into()
                });
            }
            match tokio::time::timeout(deadline - now, &mut rx).await {
                Ok(Ok(Ok(val))) => return Ok(val),
                Ok(Ok(Err(err))) => return Err(acp_error_message(&err)),
                Ok(Err(_)) => return Err("ACP request cancelled".into()),
                Err(_) => continue, // idle window elapsed; loop re-checks activity/deadline
            }
        }
    }

    /// Send a JSON-RPC *response* to a server-initiated request (used to answer
    /// a deferred `session/request_permission`).
    pub async fn respond(&self, id: Value, result: Value) {
        let msg = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let _ = self.out.send(Message::Text(msg.to_string())).await;
    }

    /// Send a JSON-RPC *notification* (no id, no response) — e.g. `session/cancel`.
    pub async fn notify(&self, method: &str, params: Value) {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let _ = self.out.send(Message::Text(msg.to_string())).await;
    }

    /// Take a deferred permission request's JSON-RPC id by its tool-call key.
    pub async fn take_permission(&self, key: &str) -> Option<Value> {
        self.permissions.lock().await.remove(key)
    }
}

/// Extract a human-readable message from a JSON-RPC error object.
fn acp_error_message(err: &Value) -> String {
    err.get("message")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("ACP error: {err}"))
}

/// Get-or-create the shared ACP client, connecting to the running goosed.
///
/// Holds a single `acp` lock across the whole check-connect-set sequence
/// (rather than re-locking around the `connect().await`) so two concurrent
/// callers that both find no client yet can't both dial a connection —
/// the second would otherwise silently orphan the first's socket and its
/// background reader/writer tasks.
pub async fn ensure_client(app: &AppHandle) -> Result<AcpClient, String> {
    let state = app.state::<AppState>();
    let mut guard = state.acp.lock().await;
    if let Some(existing) = guard.as_ref() {
        return Ok(existing.clone());
    }
    let (port, secret) = {
        let g = state.goosed.lock().unwrap();
        (g.port, g.secret_key.clone())
    };
    let port = port.ok_or("Goose isn’t running yet.")?;
    let secret = secret.ok_or("Goose isn’t running yet.")?;

    let client = AcpClient::connect(app.clone(), port, &secret).await?;
    *guard = Some(client.clone());
    Ok(client)
}
