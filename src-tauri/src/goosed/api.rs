//! ACP JSON-RPC client over a single bidirectional WebSocket.
//!
//! One connection multiplexes all sessions. Outgoing requests are matched to
//! responses by id via a pending-oneshot map; incoming notifications and
//! server-initiated requests are dispatched in [`crate::goosed::stream`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::goosed::stream;
use crate::state::AppState;

pub type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, Value>>>>>;
/// Deferred `session/request_permission` requests: tool-call id -> JSON-RPC id
/// to respond to once the user approves/denies.
pub type Perm = Arc<Mutex<HashMap<String, Value>>>;

/// Cheap-to-clone handle to the live ACP connection (holds channel + shared maps).
#[derive(Clone)]
pub struct AcpClient {
    out: mpsc::UnboundedSender<Message>,
    pending: Pending,
    permissions: Perm,
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

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if write.send(msg).await.is_err() {
                    break;
                }
            }
        });

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let permissions: Perm = Arc::new(Mutex::new(HashMap::new()));
        let client = AcpClient {
            out: out_tx.clone(),
            pending: pending.clone(),
            permissions: permissions.clone(),
            next_id: Arc::new(AtomicI64::new(0)),
        };

        let app_reader = app.clone();
        let out_reader = out_tx.clone();
        tokio::spawn(async move {
            while let Some(next) = read.next().await {
                match next {
                    Ok(Message::Text(txt)) => {
                        stream::handle_incoming(&app_reader, &out_reader, &pending, &permissions, &txt)
                            .await;
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
            tracing::warn!("ACP websocket closed");
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

    /// Send a JSON-RPC *response* to a server-initiated request (used to answer
    /// a deferred `session/request_permission`).
    pub fn respond(&self, id: Value, result: Value) {
        let msg = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let _ = self.out.send(Message::Text(msg.to_string()));
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
pub async fn ensure_client(app: &AppHandle) -> Result<AcpClient, String> {
    let state = app.state::<AppState>();
    {
        if let Some(existing) = state.acp.lock().await.as_ref() {
            return Ok(existing.clone());
        }
    }
    let (port, secret) = {
        let g = state.goosed.lock().unwrap();
        (g.port, g.secret_key.clone())
    };
    let port = port.ok_or("Goose isn’t running yet.")?;
    let secret = secret.ok_or("Goose isn’t running yet.")?;

    let client = AcpClient::connect(app.clone(), port, &secret).await?;
    *state.acp.lock().await = Some(client.clone());
    Ok(client)
}
