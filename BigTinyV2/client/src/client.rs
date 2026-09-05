//! The authenticated HTTP client.
//!
//! Three timeout tiers, carried over from Kitty's V1 client because the
//! distinction is real and easy to get wrong by using one value everywhere:
//!
//! - **Short** for control-plane calls that either answer immediately or are
//!   broken.
//! - **Long** for calls that legitimately take a while — a provider health
//!   probe reaching a sleeping Tailscale peer, an embedding batch.
//! - **None** for streams, which run as long as the model generates. A timeout
//!   here would kill a working turn mid-answer.

use std::time::Duration;

use bigtiny2_protocol::discovery::{RegisterRequest, RegisterResponse};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::error::{ClientError, Result};

/// Control-plane calls: list sessions, read a job, patch config.
const SHORT_TIMEOUT: Duration = Duration::from_secs(15);
/// Calls that do real work but still terminate: health probes, embeddings.
const LONG_TIMEOUT: Duration = Duration::from_secs(120);

/// An authenticated connection to a daemon.
#[derive(Clone)]
pub struct BigTinyClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl BigTinyClient {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            // No client-level timeout: it would apply to streams too. Timeouts
            // are set per request instead.
            http: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Register an app and return its durable key.
    ///
    /// Authorized by the handshake's registration token, not an app key — the
    /// caller has none yet, which is the whole reason it is calling. Store the
    /// returned key; it survives daemon restarts and is never recoverable from
    /// the daemon afterwards.
    pub async fn register(
        base_url: &str,
        registration_token: &str,
        app_id: &str,
        display_name: &str,
    ) -> Result<RegisterResponse> {
        let http = reqwest::Client::new();
        let resp = http
            .post(format!("{base_url}/api/apps/register"))
            .header("X-Registration-Token", registration_token)
            .timeout(SHORT_TIMEOUT)
            .json(&RegisterRequest {
                app_id: app_id.to_string(),
                display_name: display_name.to_string(),
            })
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::CONFLICT {
            // Almost always a lost secret store rather than a bug, and the
            // remedy (recover the key, or register a fresh id) depends on the
            // app -- so it gets its own variant rather than a generic 409.
            return Err(ClientError::AlreadyRegistered(app_id.to_string()));
        }
        Self::decode(resp).await
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Turn a response into a value, mapping status codes callers act on.
    async fn decode<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ClientError::Unauthorized);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ClientError::Http {
                status: status.as_u16(),
                body,
            });
        }
        resp.json::<T>()
            .await
            .map_err(|e| ClientError::Decode(e.to_string()))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, timeout: Duration) -> Result<T> {
        let resp = self
            .http
            .get(self.url(path))
            .header("X-API-Key", &self.api_key)
            .timeout(timeout)
            .send()
            .await?;
        Self::decode(resp).await
    }

    async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
        timeout: Duration,
    ) -> Result<T> {
        let resp = self
            .http
            .post(self.url(path))
            .header("X-API-Key", &self.api_key)
            .timeout(timeout)
            .json(body)
            .send()
            .await?;
        Self::decode(resp).await
    }

    // ---- sessions ---------------------------------------------------------

    pub async fn create_session(&self, name: &str) -> Result<String> {
        let v: Value = self
            .post("/api/chat/", &json!({ "name": name }), SHORT_TIMEOUT)
            .await?;
        v["session_id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| ClientError::Decode("no session_id in response".into()))
    }

    pub async fn list_sessions(&self) -> Result<Value> {
        self.get("/api/chat/", SHORT_TIMEOUT).await
    }

    pub async fn history(&self, session_id: &str) -> Result<Value> {
        self.get(&format!("/api/chat/{session_id}/history"), SHORT_TIMEOUT)
            .await
    }

    // ---- jobs -------------------------------------------------------------

    /// Submit detached work. Returns `(job_id, session_id)`.
    ///
    /// The call returns as soon as the job is durable; it does not wait for
    /// the turn. That is the point — the caller may exit and collect later.
    pub async fn submit_job(&self, prompt: &str, parent_session_id: Option<&str>) -> Result<(String, String)> {
        let mut body = json!({ "prompt": prompt });
        if let Some(parent) = parent_session_id {
            body["parent_session_id"] = json!(parent);
        }
        let v: Value = self.post("/api/jobs", &body, SHORT_TIMEOUT).await?;
        let job = v["job_id"].as_str().unwrap_or_default().to_string();
        let session = v["session_id"].as_str().unwrap_or_default().to_string();
        if job.is_empty() {
            return Err(ClientError::Decode("no job_id in response".into()));
        }
        Ok((job, session))
    }

    pub async fn job(&self, job_id: &str) -> Result<Value> {
        self.get(&format!("/api/jobs/{job_id}"), SHORT_TIMEOUT).await
    }

    pub async fn jobs(&self, status: Option<&str>) -> Result<Value> {
        let path = match status {
            Some(s) => format!("/api/jobs?status={s}"),
            None => "/api/jobs".to_string(),
        };
        self.get(&path, SHORT_TIMEOUT).await
    }

    /// Poll until a job reaches a terminal state.
    ///
    /// A convenience, not the only way to use jobs: an app that submits
    /// thousands should poll the list rather than each one. Backs off from
    /// 200ms to 2s so a short job returns quickly without a long one hammering
    /// the daemon.
    pub async fn await_job(&self, job_id: &str, timeout: Duration) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut delay = Duration::from_millis(200);
        loop {
            let job = self.job(job_id).await?;
            let status = job["status"].as_str().unwrap_or_default();
            if matches!(
                status,
                "succeeded" | "failed" | "cancelled" | "interrupted"
            ) {
                return Ok(job);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ClientError::ReadinessTimeout(timeout));
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(2));
        }
    }

    // ---- providers --------------------------------------------------------

    /// Providers this app can see, each with its live queue state.
    ///
    /// The queue fields are what let a caller pace itself against the
    /// endpoint's real slot count rather than discovering the limit as latency.
    pub async fn providers(&self) -> Result<Vec<ProviderInfo>> {
        let v: Value = self.get("/api/providers", SHORT_TIMEOUT).await?;
        let rows = v["providers"].as_array().cloned().unwrap_or_default();
        Ok(rows
            .iter()
            .map(|p| ProviderInfo {
                id: p["id"].as_str().unwrap_or_default().to_string(),
                name: p["name"].as_str().unwrap_or_default().to_string(),
                concurrency: p["queue"]["concurrency"].as_u64().unwrap_or(1) as u32,
                in_flight: p["queue"]["in_flight"].as_u64().unwrap_or(0) as u32,
                queue_depth: p["queue"]["queue_depth"].as_u64().unwrap_or(0) as usize,
                my_queue_depth: p["queue"]["my_queue_depth"].as_u64().unwrap_or(0) as usize,
                slots_source: p["queue"]["slots_source"]
                    .as_str()
                    .unwrap_or("default")
                    .to_string(),
            })
            .collect())
    }

    // ---- embeddings -------------------------------------------------------

    /// Embed a batch of texts.
    ///
    /// Batching is the caller's job only in the sense of choosing sizes: this
    /// sends one request. A pipeline embedding ten thousand chunks one
    /// round-trip at a time is the difference between usable and not, which is
    /// why the batch form exists at all.
    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let v: Value = self
            .post("/api/embeddings", &json!({ "input": texts }), LONG_TIMEOUT)
            .await?;
        let rows = v["embeddings"]
            .as_array()
            .ok_or_else(|| ClientError::Decode("no embeddings in response".into()))?;
        Ok(rows
            .iter()
            .map(|r| {
                r.as_array()
                    .map(|v| v.iter().filter_map(|f| f.as_f64().map(|f| f as f32)).collect())
                    .unwrap_or_default()
            })
            .collect())
    }

    // ---- search -----------------------------------------------------------

    pub async fn search(&self, query: &str) -> Result<Value> {
        let encoded = urlencode(query);
        self.get(&format!("/api/search?q={encoded}"), SHORT_TIMEOUT)
            .await
    }

    // ---- plugins ----------------------------------------------------------

    pub async fn plugins(&self) -> Result<Value> {
        self.get("/api/apps/me/plugins", SHORT_TIMEOUT).await
    }

    pub async fn set_plugin(&self, plugin: &str, enabled: bool) -> Result<()> {
        let resp = self
            .http
            .put(self.url(&format!("/api/apps/me/plugins/{plugin}")))
            .header("X-API-Key", &self.api_key)
            .timeout(SHORT_TIMEOUT)
            .json(&json!({ "enabled": enabled }))
            .send()
            .await?;
        let _: Value = Self::decode(resp).await?;
        Ok(())
    }
}

/// A provider plus its live queue state.
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    /// Slots this endpoint serves at once.
    pub concurrency: u32,
    pub in_flight: u32,
    /// Waiters across every app.
    pub queue_depth: usize,
    /// This app's own waiters. Distinct from `queue_depth`, so a caller can
    /// tell "the endpoint is busy" from "*I* have a backlog".
    pub my_queue_depth: usize,
    /// `configured` | `probed` | `default`.
    pub slots_source: String,
}

/// Percent-encode a query value.
///
/// Hand-rolled rather than pulling a crate in for one call site: the client's
/// dependency list is part of its appeal.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencoding_escapes_what_would_break_a_query() {
        assert_eq!(urlencode("hello world"), "hello%20world");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencode("safe-._~"), "safe-._~");
        // Multi-byte input is encoded per byte, which is what the spec wants.
        assert_eq!(urlencode("é"), "%C3%A9");
    }

    #[test]
    fn a_client_carries_its_base_url() {
        let c = BigTinyClient::new("http://127.0.0.1:1234", "key");
        assert_eq!(c.base_url(), "http://127.0.0.1:1234");
        assert_eq!(c.url("/api/health"), "http://127.0.0.1:1234/api/health");
    }
}
