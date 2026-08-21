use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::MCPServerError;
use crate::models::mcp::ToolDefinition;

use super::tools::extract_content_from_json_sized;

const PROTOCOL_VERSION: &str = "2024-11-05";
const CLIENT_NAME: &str = "bigtiny";
const CLIENT_VERSION: &str = "0.1.0";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Hand-rolled client for BigTiny's existing "sse" MCP transport, which is
/// *not* the spec-compliant HTTP+SSE transport (that would open a GET stream,
/// wait for an `endpoint` event, then receive replies asynchronously over that
/// stream). Instead it POSTs JSON-RPC requests to `{url}` with `/sse` replaced
/// by `/message`, expecting the reply synchronously in the POST response body.
/// A spec-correct SSE client would not interoperate with servers built against
/// this daemon's existing behavior, so this is intentionally not built on
/// `rmcp`'s SSE transport. The spec's GET `/sse` listener is not implemented —
/// confirmed vestigial in the Python reference (feeds a queue nothing reads).
pub struct SseTransport {
    client: reqwest::Client,
    message_url: String,
    request_id: AtomicI64,
}

/// Rewrite a configured `sse` server URL's trailing `/sse` segment to
/// `/message`. Only touches the *trailing* segment — a naive
/// `str::replace`/`replacen("/sse", "/message")` would also mangle a URL
/// where `/sse` happens to appear earlier in the path (e.g.
/// `https://host/api/sse/events`), rewriting it to
/// `https://host/api/message/events` instead of leaving the rest of the
/// path alone. Falls back to Python's original whole-string replace only
/// for a URL that doesn't end in `/sse` as expected, rather than guessing
/// further.
fn sse_url_to_message_url(url: &str) -> String {
    match url.strip_suffix("/sse") {
        Some(base) => format!("{base}/message"),
        None => url.replace("/sse", "/message"),
    }
}

impl SseTransport {
    pub async fn connect(url: String, headers: Option<Value>) -> Result<Self, MCPServerError> {
        let mut header_map = reqwest::header::HeaderMap::new();
        if let Some(obj) = headers.as_ref().and_then(|v| v.as_object()) {
            for (k, v) in obj {
                // Silently dropping a header means the server is dialled
                // with NO auth: every call 401s while the UI still says
                // "connected", or worse the server accepts the
                // unauthenticated session. Log it.
                let name = reqwest::header::HeaderName::from_bytes(k.as_bytes());
                let val = v.as_str().and_then(|s| s.parse().ok());
                match (name, val) {
                    (Ok(name), Some(val)) => {
                        header_map.insert(name, val);
                    }
                    _ => tracing::warn!(
                        "dropping invalid configured header {k:?} for sse MCP server at {url}"
                    ),
                }
            }
        }
        let client = reqwest::Client::builder()
            .default_headers(header_map)
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| MCPServerError::Transport(e.to_string()))?;

        let message_url = sse_url_to_message_url(&url);

        let transport = Self {
            client,
            message_url,
            request_id: AtomicI64::new(0),
        };

        transport
            .send_request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": CLIENT_NAME, "version": CLIENT_VERSION},
                }),
            )
            .await?;
        transport
            .send_notification("notifications/initialized", json!({}))
            .await?;

        Ok(transport)
    }

    fn next_id(&self) -> i64 {
        self.request_id.fetch_add(1, Ordering::SeqCst) + 1
    }

    async fn send_request(&self, method: &str, params: Value) -> Result<Value, MCPServerError> {
        let id = self.next_id();
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let resp = self
            .client
            .post(&self.message_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| MCPServerError::Transport(e.to_string()))?;
        // A non-2xx reply used to be parsed as a JSON response anyway — a
        // 500/HTML error body would fail the `.json()` parse with a confusing
        // "transport error" (or, for an HTML page that happens to start with
        // `{`, be misread as a *successful* result). Check the status first.
        let status_code = resp.status().as_u16();
        if !(200..300).contains(&status_code) {
            let text = resp.text().await.unwrap_or_default();
            return Err(MCPServerError::Transport(format!(
                "MCP request '{method}' failed with HTTP {status_code}: {text}"
            )));
        }
        let data: Value = resp
            .json()
            .await
            .map_err(|e| MCPServerError::Transport(e.to_string()))?;
        // Correlate the reply with the request. A server that answers with
        // someone else's `id` (or with a body that isn't the reply at all)
        // used to be accepted as this call's result — under concurrent tool
        // calls that silently returns one tool's output as another's.
        match data.get("id") {
            Some(Value::Number(n)) if n.as_i64() == Some(id) => {}
            Some(other) => {
                return Err(MCPServerError::Transport(format!(
                    "MCP request '{method}' got a reply for id {other} (expected {id})"
                )))
            }
            // An error reply may legitimately carry a null id (JSON-RPC
            // allows it when the request couldn't be parsed), so only demand
            // a matching id on something claiming to be a result.
            None if data.get("error").is_none() => {
                return Err(MCPServerError::Transport(format!(
                    "MCP request '{method}' got a reply with no id"
                )))
            }
            None => {}
        }
        if let Some(err) = data.get("error") {
            let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
            let message = err
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_string();
            return Err(MCPServerError::Protocol { code, message });
        }
        // A JSON-RPC reply carries exactly one of `result`/`error`. Defaulting
        // a missing `result` to `{}` reported a protocol violation (or a 200
        // with an empty body) as a *successful* call with no content — the
        // model then saw an empty tool result instead of an error it could
        // react to.
        data.get("result").cloned().ok_or_else(|| {
            MCPServerError::Transport(format!(
                "MCP request '{method}' reply had neither a result nor an error"
            ))
        })
    }

    async fn send_notification(&self, method: &str, params: Value) -> Result<(), MCPServerError> {
        let body = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let resp = self
            .client
            .post(&self.message_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| MCPServerError::Transport(e.to_string()))?;
        // Previously the response (and its status) was simply dropped, so a
        // failed notification looked like success — the server could be
        // rejecting every message and the client would never notice.
        let status_code = resp.status().as_u16();
        if !(200..300).contains(&status_code) {
            let text = resp.text().await.unwrap_or_default();
            return Err(MCPServerError::Transport(format!(
                "MCP notification '{method}' failed with HTTP {status_code}: {text}"
            )));
        }
        Ok(())
    }

    pub async fn list_tools(&self, server_id: &str) -> Result<Vec<ToolDefinition>, MCPServerError> {
        let result = self.send_request("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(tools
            .into_iter()
            .map(|t| ToolDefinition {
                name: t
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                description: t
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                input_schema: t.get("inputSchema").cloned().unwrap_or_else(|| json!({})),
                server_id: server_id.to_string(),
            })
            .collect())
    }

    /// Returns `(is_error, content_text, raw_bytes)`; the caller
    /// (`client.rs`) wraps this into a `ToolResult`, matching the rmcp-backed
    /// transports' shape. `raw_bytes` is the pre-cap size, so
    /// `output_size_bytes` stays truthful about what the server actually
    /// produced even when the text was capped during extraction.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        args: &Value,
    ) -> Result<(bool, String, usize), MCPServerError> {
        let result = self
            .send_request("tools/call", json!({"name": tool_name, "arguments": args}))
            .await?;
        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // Same reasoning as the missing-`result` case: a `tools/call` reply
        // with no `content` is malformed, not an empty success.
        let content = result.get("content").ok_or_else(|| {
            MCPServerError::Transport(format!("tool '{tool_name}' reply had no content array"))
        })?;
        let (text, raw_bytes) = extract_content_from_json_sized(content);
        Ok((is_error, text, raw_bytes))
    }

    /// Stateless POST-only transport — nothing to tear down.
    pub async fn shutdown(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_rewrites_the_trailing_sse_segment() {
        assert_eq!(
            sse_url_to_message_url("https://host:1234/sse"),
            "https://host:1234/message"
        );
    }

    #[test]
    fn only_the_final_sse_occurrence_matters_when_it_is_the_trailing_segment() {
        // "/sse" appearing earlier in the path (e.g. a hostname/subpath
        // that happens to contain it) is untouched as long as the URL
        // still ends in the conventional trailing "/sse" segment — only
        // that final segment is rewritten.
        assert_eq!(
            sse_url_to_message_url("https://sse.example.com/mcp/sse"),
            "https://sse.example.com/mcp/message"
        );
    }

    /// Regression (WS4-3): a non-2xx reply to the `initialize` request used to
    /// be misread as success (dropped response status); it must fail `connect`.
    #[tokio::test]
    async fn non_success_status_fails_connect() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/message")
            .with_status(500)
            .with_body("boom")
            .expect(1)
            .create_async()
            .await;

        let result = SseTransport::connect(format!("{}/sse", server.url()), None).await;
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected connect to fail on a non-2xx initialize"),
        };
        match err {
            MCPServerError::Transport(msg) => {
                assert!(
                    msg.contains("HTTP 500"),
                    "transport error should mention the HTTP status: {msg}"
                );
            }
            other => panic!("expected a Transport error, got {other:?}"),
        }
        mock.assert_async().await;
    }
}
