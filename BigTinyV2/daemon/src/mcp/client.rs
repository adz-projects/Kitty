use std::time::Duration;

use rmcp::model::{
    CallToolRequest, CallToolRequestParam, ClientCapabilities, ClientInfo, ClientRequest,
    Implementation, ProtocolVersion, ServerResult,
};
use rmcp::service::{PeerRequestOptions, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use tokio::process::Command;

use crate::error::MCPServerError;
use crate::models::mcp::{MCPServerConfig, ToolDefinition, ToolResult, TransportType};

use super::child_transport::ChildProcessTransport;
use super::rw_transport::HardenedRwTransport;
use super::sse_transport::SseTransport;
use super::tools::{extract_content_from_rmcp_sized, truncate_output};

const CLIENT_NAME: &str = "bigtiny";
const CLIENT_VERSION: &str = "0.1.0";
/// Duplex pipe buffer size for `MCPServerClient::connect_in_process` — large
/// enough that a single tool call's request/response doesn't deadlock
/// waiting for the other side to drain a full buffer before it can write the
/// rest of a message (`tokio::io::duplex` applies backpressure once this
/// fills, which is exactly the read/write coupling a real stdio pipe would
/// also impose, but a too-small buffer would make it bite far more often).
const IN_PROCESS_BUF_SIZE: usize = 256 * 1024;
/// Slack on the outer `execute_tool` bound so the inner, *cancelling* rmcp
/// timeout fires first on the transports that support it — otherwise the
/// outer one would win the race and we would be back to abandoning the call
/// instead of cancelling it.
const TIMEOUT_GRACE: Duration = Duration::from_secs(2);

fn client_info() -> ClientInfo {
    ClientInfo {
        protocol_version: ProtocolVersion::V_2024_11_05,
        capabilities: ClientCapabilities::default(),
        client_info: Implementation {
            name: CLIENT_NAME.into(),
            version: CLIENT_VERSION.into(),
            ..Default::default()
        },
    }
}

enum ClientHandle {
    Rmcp(RunningService<RoleClient, ClientInfo>),
    Sse(SseTransport),
}

/// One connected MCP server. Wraps either an `rmcp`-managed session (stdio,
/// streamable_http — both spec-compliant, both handle the
/// initialize/notifications-initialized/tools-list handshake internally as
/// part of `.serve()`) or the hand-rolled `SseTransport` for BigTiny's
/// existing non-spec "sse" transport.
pub struct MCPServerClient {
    server_id: String,
    handle: ClientHandle,
    tools: Vec<ToolDefinition>,
}

impl MCPServerClient {
    pub async fn connect(config: &MCPServerConfig) -> Result<Self, MCPServerError> {
        let server_id = if config.id.is_empty() {
            uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
        } else {
            config.id.clone()
        };

        let handle = match config.transport {
            TransportType::Stdio => Self::connect_stdio(config).await?,
            TransportType::StreamableHttp => Self::connect_streamable_http(config).await?,
            TransportType::Sse => {
                let url = config.url.clone().ok_or_else(|| {
                    MCPServerError::Generic("sse transport requires a url".into())
                })?;
                let sse = SseTransport::connect(url, config.headers.clone()).await?;
                ClientHandle::Sse(sse)
            }
            // Routed by `mcp::manager::connect_server` to `mcp::builtin` +
            // `connect_in_process` instead — there's no `MCPServerConfig`
            // field this method could execute or dial for this transport,
            // only a logical name to look up in that registry.
            TransportType::InProcess => {
                return Err(MCPServerError::Generic(
                    "in_process transport must be connected via mcp::builtin, not MCPServerClient::connect".into(),
                ));
            }
        };

        let mut client = Self {
            server_id,
            handle,
            tools: Vec::new(),
        };
        client.refresh_tools().await?;
        Ok(client)
    }

    async fn connect_stdio(config: &MCPServerConfig) -> Result<ClientHandle, MCPServerError> {
        let command = config
            .command
            .clone()
            .ok_or_else(|| MCPServerError::Generic("stdio transport requires a command".into()))?;
        let args = config.args.clone().unwrap_or_default();
        let env_overlay: std::collections::HashMap<String, String> = config
            .env
            .as_ref()
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        let mut cmd = Command::new(&command);
        cmd.args(&args);
        for (k, v) in &env_overlay {
            cmd.env(k, v);
        }
        // `ChildProcessTransport`, not rmcp's `TokioChildProcess`: same child
        // lifecycle (graceful stdin-close, 3s grace, reaping kill), but framed
        // by `rw_transport::HardenedRwTransport` so one non-JSON log line from
        // a third-party server can't take its whole tool set offline, an
        // unbounded stdout can't OOM the daemon, and a child that stops
        // draining its stdin can't wedge the transport forever.
        let child = ChildProcessTransport::spawn(cmd)
            .map_err(|e| MCPServerError::Transport(e.to_string()))?;

        let running = client_info()
            .serve(child)
            .await
            .map_err(|e| MCPServerError::Transport(e.to_string()))?;
        Ok(ClientHandle::Rmcp(running))
    }

    /// Connects to an MCP server hosted as a task in *this same process*
    /// rather than as a child process or over HTTP.
    ///
    /// This is the transport an exec()-restricted host needs: Android 10+
    /// blocks `exec()` of binaries in app-writable directories, so a tool
    /// server that ships as a stdio subprocess on desktop (`kitty-tools`,
    /// today) can't be spawned as a child there. `serve` is handed the
    /// server-side end of an in-memory duplex pipe and spawned as its own
    /// task; it's expected to speak newline-delimited JSON-RPC 2.0 over that
    /// pipe exactly as a stdio child would over its own stdin/stdout —
    /// `rmcp`'s client side can't tell the difference (both a real child's
    /// stdio and a `DuplexStream` satisfy the same `AsyncRead + AsyncWrite`
    /// blanket transport impl), and neither can a server built against some
    /// *other* `rmcp` version than this crate's (`kitty-tools` pins a
    /// different major version), since the two ends only ever exchange
    /// serialized bytes on the wire, never Rust types.
    ///
    /// If `serve` returns (or panics) before the handshake completes,
    /// `.serve(client_side)` below fails with a transport error the same way
    /// a child process that exits immediately would — there is no special
    /// case needed for that.
    pub async fn connect_in_process<F, Fut>(
        server_id: String,
        serve: F,
    ) -> Result<Self, MCPServerError>
    where
        F: FnOnce(tokio::io::DuplexStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let (client_side, server_side) = tokio::io::duplex(IN_PROCESS_BUF_SIZE);
        // Observe the serve task rather than dropping its `JoinHandle`: a
        // panicking in-process tool used to vanish silently, leaving the
        // client to fail every later call with an opaque transport error and
        // no clue in the log about what actually died.
        let observed_id = server_id.clone();
        tokio::spawn(async move {
            if let Err(e) = tokio::spawn(serve(server_side)).await {
                if e.is_panic() {
                    tracing::error!(
                        "in-process MCP server {observed_id} panicked; its transport is now dead: {e}"
                    );
                } else {
                    tracing::warn!("in-process MCP server {observed_id} task ended: {e}");
                }
            }
        });

        let (read, write) = tokio::io::split(client_side);
        let running = client_info()
            .serve(HardenedRwTransport::new(read, write))
            .await
            .map_err(|e| MCPServerError::Transport(e.to_string()))?;

        let mut client = Self {
            server_id,
            handle: ClientHandle::Rmcp(running),
            tools: Vec::new(),
        };
        client.refresh_tools().await?;
        Ok(client)
    }

    async fn connect_streamable_http(
        config: &MCPServerConfig,
    ) -> Result<ClientHandle, MCPServerError> {
        let url = config.url.clone().ok_or_else(|| {
            MCPServerError::Generic("streamable_http transport requires a url".into())
        })?;

        let mut header_map = reqwest::header::HeaderMap::new();
        header_map.insert(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream"
                .parse()
                .expect("static header value is valid"),
        );
        if let Some(headers) = config.headers.as_ref().and_then(|v| v.as_object()) {
            for (k, v) in headers {
                // Silently dropping an invalid header (bad name, non-Latin-1
                // value) means the server connects with NO auth at all — the
                // user sees "connected" while every call 401s, or worse the
                // server accepts the unauthenticated session. Log loudly.
                let name = reqwest::header::HeaderName::from_bytes(k.as_bytes());
                let val = v.as_str().and_then(|s| s.parse().ok());
                match (name, val) {
                    (Ok(name), Some(val)) => {
                        header_map.insert(name, val);
                    }
                    _ => {
                        tracing::warn!(
                            "dropping invalid configured header {k:?} for MCP server {:?}",
                            config.name
                        );
                    }
                }
            }
        }
        let http_client = reqwest::Client::builder()
            .default_headers(header_map)
            .build()
            .map_err(|e| MCPServerError::Transport(e.to_string()))?;

        let transport = StreamableHttpClientTransport::with_client(
            http_client,
            StreamableHttpClientTransportConfig::with_uri(url),
        );

        let running = client_info()
            .serve(transport)
            .await
            .map_err(|e| MCPServerError::Transport(e.to_string()))?;
        Ok(ClientHandle::Rmcp(running))
    }

    async fn refresh_tools(&mut self) -> Result<(), MCPServerError> {
        self.tools = match &self.handle {
            ClientHandle::Rmcp(running) => running
                .list_all_tools()
                .await
                .map_err(|e| MCPServerError::Transport(e.to_string()))?
                .into_iter()
                .map(|t| {
                    // A failed schema serialization previously registered the
                    // tool with an empty `{}` schema — and an empty schema
                    // makes `validate_tool_args` fail OPEN, so the tool became
                    // callable with arbitrary arguments, bypassing the schema
                    // validation the server advertised. Fail the connect
                    // instead of silently weakening that.
                    let input_schema = serde_json::to_value(&*t.input_schema).map_err(|e| {
                        MCPServerError::Generic(format!(
                            "tool {:?} input_schema not serializable: {e}",
                            t.name
                        ))
                    })?;
                    Ok(ToolDefinition {
                        name: t.name.to_string(),
                        description: t.description.map(|d| d.to_string()).unwrap_or_default(),
                        input_schema,
                        server_id: self.server_id.clone(),
                    })
                })
                .collect::<Result<Vec<_>, MCPServerError>>()?,
            ClientHandle::Sse(sse) => sse.list_tools(&self.server_id).await?,
        };
        // Compile-check every advertised schema at connect: the call path
        // (`mcp::tools::validate_tool_args`) goes through `jsonschema`, which
        // *panics* on a schema it can't compile — unwinding the whole turn
        // task. Fail the connect instead, the same treatment the
        // un-serializable case above already gets, so an uncompilable schema
        // can never reach a tool call.
        for t in &self.tools {
            if t.input_schema.is_null() {
                continue;
            }
            jsonschema::validator_for(&t.input_schema).map_err(|e| {
                MCPServerError::Generic(format!(
                    "tool {:?} input_schema is not a valid JSON Schema: {e}",
                    t.name
                ))
            })?;
        }
        Ok(())
    }

    pub fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    /// Whether this client's transport is still usable. Cheap and
    /// non-blocking — it reads rmcp's own closed flag, which flips when the
    /// serve loop ends (child exited, pipe broke, or `HardenedRwTransport`
    /// declared a wedged peer dead). Drives `MCPManager`'s health watcher.
    ///
    /// The hand-rolled SSE transport is stateless request/response HTTP with
    /// no session to lose, so it has no closed state to report and is always
    /// "alive"; a broken endpoint there surfaces as a failing call instead.
    pub fn is_transport_alive(&self) -> bool {
        match &self.handle {
            ClientHandle::Rmcp(running) => !running.peer().is_transport_closed(),
            ClientHandle::Sse(_) => true,
        }
    }

    /// Never returns an `Err` — every failure mode maps to a
    /// `ToolResult { is_error: true, .. }`. Load-bearing: callers run tool
    /// calls concurrently and one failing call must not cancel its siblings.
    pub async fn execute_tool(
        &self,
        tool_name: &str,
        args: &Value,
        timeout: Duration,
    ) -> ToolResult {
        let tool_call_id = format!(
            "{tool_name}_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        let start = std::time::Instant::now();
        let arguments = args.as_object().cloned();

        let call = async {
            match &self.handle {
                ClientHandle::Rmcp(running) => {
                    // Deliberately not `running.call_tool(..)`: that sends the
                    // request with no options, so a timeout on our side just
                    // drops the caller's future. The server-side tool keeps
                    // running (duplicate side effects when the model retries)
                    // and rmcp leaks the pending responder until a reply that
                    // may never come. Handing rmcp the timeout instead makes
                    // it emit `notifications/cancelled` and drop the responder.
                    let request = ClientRequest::CallToolRequest(CallToolRequest {
                        method: Default::default(),
                        params: CallToolRequestParam {
                            name: tool_name.to_string().into(),
                            arguments,
                        },
                        extensions: Default::default(),
                    });
                    let handle = running
                        .peer()
                        .send_cancellable_request(
                            request,
                            PeerRequestOptions {
                                timeout: Some(timeout),
                                meta: None,
                            },
                        )
                        .await
                        .map_err(|e| MCPServerError::Transport(e.to_string()))?;
                    let result = match handle.await_response().await {
                        Ok(ServerResult::CallToolResult(result)) => result,
                        Ok(_) => {
                            return Err(MCPServerError::Transport(
                                "server answered tools/call with the wrong result type".into(),
                            ))
                        }
                        Err(rmcp::service::ServiceError::Timeout { .. }) => {
                            return Err(MCPServerError::Timeout(timeout.as_secs_f64()))
                        }
                        Err(e) => return Err(MCPServerError::Transport(e.to_string())),
                    };
                    let is_error = result.is_error.unwrap_or(false);
                    let (content, raw_bytes) = extract_content_from_rmcp_sized(&result.content);
                    Ok::<(bool, String, usize), MCPServerError>((is_error, content, raw_bytes))
                }
                ClientHandle::Sse(sse) => sse.call_tool(tool_name, args).await,
            }
        };

        // Outer belt-and-braces bound. The rmcp path already cancels at
        // `timeout`; this catches the transports that don't (SSE) and any
        // stall *before* the request reaches rmcp's pending map. The grace
        // lets the inner, cancelling timeout win the race and report the
        // clearer error.
        match tokio::time::timeout(timeout + TIMEOUT_GRACE, call).await {
            Ok(Ok((is_error, content, raw_bytes))) => {
                // Clamp, don't wrap: `raw_bytes as i32` silently goes
                // negative past 2 GB, which then lands in the DB as a
                // nonsense negative size.
                let output_size_bytes = raw_bytes.min(i32::MAX as usize) as i32;
                let (content, truncated) = truncate_output(&content);
                ToolResult {
                    content,
                    tool_call_id,
                    duration_ms: start.elapsed().as_millis() as i32,
                    output_size_bytes,
                    is_error,
                    truncated,
                }
            }
            // The rmcp path's own (cancelling) timeout. Reported with the same
            // wording as the outer one so callers and the model can't tell
            // which bound fired — only that the tool ran out of time.
            Ok(Err(MCPServerError::Timeout(secs))) => ToolResult {
                content: format!("[Tool '{tool_name}' timed out after {secs:.0}s]"),
                tool_call_id,
                duration_ms: start.elapsed().as_millis() as i32,
                output_size_bytes: 0,
                is_error: true,
                truncated: false,
            },
            Ok(Err(e)) => ToolResult {
                content: format!("[Tool '{tool_name}' error: {e}]"),
                tool_call_id,
                duration_ms: start.elapsed().as_millis() as i32,
                output_size_bytes: 0,
                is_error: true,
                truncated: false,
            },
            Err(_) => ToolResult {
                content: format!(
                    "[Tool '{tool_name}' timed out after {}s]",
                    timeout.as_secs()
                ),
                tool_call_id,
                duration_ms: start.elapsed().as_millis() as i32,
                output_size_bytes: 0,
                is_error: true,
                truncated: false,
            },
        }
    }

    /// Graceful shutdown. For the rmcp-backed transports this closes the
    /// underlying transport (for stdio: SIGTERM-equivalent close then a grace
    /// period before kill, handled by `TokioChildProcess::graceful_shutdown`).
    /// Consumes `self` because `RunningService::cancel` takes ownership; the
    /// manager recovers ownership via `Arc::try_unwrap` when it evicts a
    /// server.
    pub async fn shutdown(self) {
        match self.handle {
            ClientHandle::Rmcp(running) => {
                let _ = running.cancel().await;
            }
            ClientHandle::Sse(sse) => sse.shutdown().await,
        }
    }
}

#[cfg(test)]
mod in_process_tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// A hand-rolled newline-delimited JSON-RPC server run entirely over the
    /// duplex pipe `connect_in_process` hands it — deliberately not built on
    /// `rmcp`'s own server support (which this crate doesn't even depend on;
    /// see `Cargo.toml`'s `client`-only feature list), mirroring
    /// `tests/bin/fake_mcp_server.rs`'s stdio version line for line. The
    /// point of both is the same: prove the client side can't tell a duplex
    /// pipe from a child's stdio, and doesn't need to.
    async fn fake_server(stream: tokio::io::DuplexStream) {
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut lines = BufReader::new(read_half).lines();

        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let msg: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let id = msg.get("id").cloned();

            let response = match method {
                "initialize" => id.map(|id| {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": "2024-11-05",
                            "capabilities": {},
                            "serverInfo": {"name": "fake-in-process-server", "version": "0.1.0"}
                        }
                    })
                }),
                "notifications/initialized" => None,
                "tools/list" => id.map(|id| {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [{
                                "name": "echo_tool",
                                "description": "Echoes text back",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {"text": {"type": "string"}},
                                    "required": ["text"]
                                }
                            }]
                        }
                    })
                }),
                "tools/call" => {
                    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
                    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let args = params
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    id.map(|id| match name {
                        "echo_tool" => {
                            let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "content": [{"type": "text", "text": text}],
                                    "isError": false
                                }
                            })
                        }
                        other => json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {"code": -32601, "message": format!("Unknown tool: {other}")}
                        }),
                    })
                }
                _ => None,
            };

            if let Some(response) = response {
                let mut out = serde_json::to_vec(&response).unwrap();
                out.push(b'\n');
                if write_half.write_all(&out).await.is_err() {
                    break;
                }
            }
        }
    }

    #[tokio::test]
    async fn connect_in_process_completes_the_handshake_and_lists_tools() {
        let client = MCPServerClient::connect_in_process("test-server".to_string(), fake_server)
            .await
            .expect("in-process connect should succeed");

        assert_eq!(client.server_id(), "test-server");
        let names: Vec<&str> = client.tools().iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["echo_tool"]);
    }

    #[tokio::test]
    async fn connect_in_process_round_trips_a_tool_call() {
        let client = MCPServerClient::connect_in_process("test-server".to_string(), fake_server)
            .await
            .unwrap();

        let result = client
            .execute_tool(
                "echo_tool",
                &json!({"text": "hello from in-process"}),
                Duration::from_secs(5),
            )
            .await;

        assert!(!result.is_error);
        assert_eq!(result.content, "hello from in-process");
    }

    #[tokio::test]
    async fn connect_in_process_never_errors_on_an_unknown_tool() {
        let client = MCPServerClient::connect_in_process("test-server".to_string(), fake_server)
            .await
            .unwrap();

        // Matches the stdio transport's contract exercised by
        // `tests/mcp_never_throws.rs`: a server-side error becomes
        // `ToolResult { is_error: true, .. }`, never a panic or an `Err`
        // that could cancel a sibling concurrent tool call.
        let result = client
            .execute_tool("no_such_tool", &json!({}), Duration::from_secs(5))
            .await;

        assert!(result.is_error);
    }

    /// A server advertising a schema `jsonschema` can't compile (`"type":
    /// 123`). Regression: the schema used to be checked only for
    /// *serializability* at connect, then `jsonschema::validate` panicked on
    /// it at call time, unwinding the turn task. The connect must fail
    /// instead, like the un-serializable case.
    async fn fake_server_bad_schema(stream: tokio::io::DuplexStream) {
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut lines = BufReader::new(read_half).lines();

        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let msg: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let id = msg.get("id").cloned();

            let response = match method {
                "initialize" => id.map(|id| {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": "2024-11-05",
                            "capabilities": {},
                            "serverInfo": {"name": "fake-bad-schema-server", "version": "0.1.0"}
                        }
                    })
                }),
                "notifications/initialized" => None,
                "tools/list" => id.map(|id| {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [{
                                "name": "broken_tool",
                                "description": "Advertises an uncompilable schema",
                                "inputSchema": {"type": 123}
                            }]
                        }
                    })
                }),
                _ => None,
            };

            if let Some(response) = response {
                let mut out = serde_json::to_vec(&response).unwrap();
                out.push(b'\n');
                if write_half.write_all(&out).await.is_err() {
                    break;
                }
            }
        }
    }

    #[tokio::test]
    async fn connect_fails_on_an_uncompilable_advertised_schema() {
        let result =
            MCPServerClient::connect_in_process("bad-schema".to_string(), fake_server_bad_schema)
                .await;
        let err = result
            .err()
            .expect("connect must fail on an uncompilable schema");
        assert!(
            err.to_string().contains("not a valid JSON Schema"),
            "error should name the schema problem, got: {err}"
        );
    }
}
