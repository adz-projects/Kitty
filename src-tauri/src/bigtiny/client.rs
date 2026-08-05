//! Thin HTTP client for the BigTiny daemon: base URL + `X-API-Key` header +
//! JSON helpers with user-safe error mapping.

use std::time::Duration;

use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::state::AppState;

/// Upper bound on any non-stream BigTiny call. The daemon lives on localhost,
/// so a call that hasn't answered in 10s is effectively wedged (the scheduler,
/// self-heal loop, and `clear_all_sessions` all sit behind this) — a stalled
/// daemon must never hang a caller forever. The streaming `/send` call is
/// deliberately excluded (`request_stream`) and governed by its own idle-only
/// deadline instead, so long, actively-streaming turns are unaffected.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Cheap-to-clone handle to the running BigTiny daemon.
#[derive(Clone)]
pub struct BigTinyClient {
    http: reqwest::Client,
    base: String,
    secret: Option<String>,
}

/// Build a client from the spawned daemon's port/secret in `AppState`.
pub fn ensure_client(app: &AppHandle) -> Result<BigTinyClient, String> {
    let state = app.state::<AppState>();
    let handle = state.bigtiny.lock().unwrap();
    let port = handle.port.ok_or("BigTiny isn’t running yet.")?;
    Ok(BigTinyClient {
        http: crate::util::http_client(),
        base: format!("http://127.0.0.1:{port}"),
        secret: handle.secret_key.clone(),
    })
}

impl BigTinyClient {
    /// A request builder for `path` (e.g. `/api/chat/`), with auth attached
    /// and a bounded total timeout so every helper inherits it.
    pub fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.base_request(method, path)
            .timeout(DEFAULT_REQUEST_TIMEOUT)
    }

    /// Like [`Self::request`], but without the default total-request timeout —
    /// for SSE streams (`/send`), whose duration is bounded by the idle-only
    /// deadline in `stream::run_stream`. A total cap here would truncate a
    /// long, actively-streaming turn, which is exactly what the idle deadline
    /// exists to avoid.
    pub fn request_stream(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.base_request(method, path)
    }

    fn base_request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut req = self.http.request(method, format!("{}{path}", self.base));
        if let Some(secret) = &self.secret {
            req = req.header("X-API-Key", secret);
        }
        req
    }

    pub async fn get_json(&self, path: &str) -> Result<Value, String> {
        json_response(self.request(reqwest::Method::GET, path).send().await).await
    }

    pub async fn post_json(&self, path: &str, body: &Value) -> Result<Value, String> {
        json_response(
            self.request(reqwest::Method::POST, path)
                .json(body)
                .send()
                .await,
        )
        .await
    }

    pub async fn patch_json(&self, path: &str, body: &Value) -> Result<Value, String> {
        json_response(
            self.request(reqwest::Method::PATCH, path)
                .json(body)
                .send()
                .await,
        )
        .await
    }

    pub async fn delete(&self, path: &str) -> Result<Value, String> {
        json_response(self.request(reqwest::Method::DELETE, path).send().await).await
    }
}

/// Map a reqwest response to JSON, folding transport + HTTP errors into one
/// user-safe string (the detailed body is included — BigTiny's error bodies
/// are short JSON like `{"detail": "Session not found"}`).
async fn json_response(result: Result<reqwest::Response, reqwest::Error>) -> Result<Value, String> {
    let resp = result.map_err(|e| format!("BigTiny request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("BigTiny error ({status}): {body}"));
    }
    resp.json::<Value>()
        .await
        .map_err(|e| format!("BigTiny returned invalid JSON: {e}"))
}
