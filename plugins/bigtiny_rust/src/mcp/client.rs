use std::time::Duration;

use rmcp::model::{
    CallToolRequestParam, ClientCapabilities, ClientInfo, Implementation, ProtocolVersion,
};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use tokio::process::Command;

use crate::error::MCPServerError;
use crate::models::mcp::{MCPServerConfig, ToolDefinition, ToolResult, TransportType};

use super::sse_transport::SseTransport;
use super::tools::{extract_content_from_rmcp, truncate_output};

const CLIENT_NAME: &str = "bigtiny";
const CLIENT_VERSION: &str = "0.1.0";

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

        let child = TokioChildProcess::new(Command::new(&command).configure(|cmd| {
            cmd.args(&args);
            for (k, v) in &env_overlay {
                cmd.env(k, v);
            }
        }))
        .map_err(|e| MCPServerError::Transport(e.to_string()))?;

        let running = client_info()
            .serve(child)
            .await
            .map_err(|e| MCPServerError::Transport(e.to_string()))?;
        Ok(ClientHandle::Rmcp(running))
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
                if let (Ok(name), Some(val)) = (
                    reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                    v.as_str(),
                ) {
                    if let Ok(val) = val.parse() {
                        header_map.insert(name, val);
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
                .map(|t| ToolDefinition {
                    name: t.name.to_string(),
                    description: t.description.map(|d| d.to_string()).unwrap_or_default(),
                    input_schema: serde_json::to_value(&*t.input_schema)
                        .unwrap_or_else(|_| serde_json::json!({})),
                    server_id: self.server_id.clone(),
                })
                .collect(),
            ClientHandle::Sse(sse) => sse.list_tools(&self.server_id).await?,
        };
        Ok(())
    }

    pub fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
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
                    let result = running
                        .call_tool(CallToolRequestParam {
                            name: tool_name.to_string().into(),
                            arguments,
                        })
                        .await
                        .map_err(|e| MCPServerError::Transport(e.to_string()))?;
                    let is_error = result.is_error.unwrap_or(false);
                    let content = extract_content_from_rmcp(&result.content);
                    Ok::<(bool, String), MCPServerError>((is_error, content))
                }
                ClientHandle::Sse(sse) => sse.call_tool(tool_name, args).await,
            }
        };

        match tokio::time::timeout(timeout, call).await {
            Ok(Ok((is_error, content))) => {
                let output_size_bytes = content.len() as i32;
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
    pub async fn shutdown(self) {
        match self.handle {
            ClientHandle::Rmcp(running) => {
                let _ = running.cancel().await;
            }
            ClientHandle::Sse(sse) => sse.shutdown().await,
        }
    }
}
