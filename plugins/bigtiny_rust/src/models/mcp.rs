use chrono::DateTime;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum TransportType {
    #[default]
    Stdio,
    Sse,
    StreamableHttp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPServerConfig {
    pub id: String,
    pub name: String,
    pub transport: TransportType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<serde_json::Value>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

impl MCPServerConfig {
    pub fn new(name: impl Into<String>, transport: TransportType) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            transport,
            command: None,
            args: None,
            url: None,
            env: None,
            headers: None,
            status: "disconnected".into(),
            error_message: None,
            created_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub server_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: String,
    pub tool_call_id: String,
    pub duration_ms: i32,
    pub output_size_bytes: i32,
    pub is_error: bool,
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcp_server_new() {
        let cfg = MCPServerConfig::new("test-server", TransportType::Sse);
        assert_eq!(cfg.name, "test-server");
        assert_eq!(cfg.transport, TransportType::Sse);
    }

    #[test]
    fn test_transport_type_serde() {
        let parsed: TransportType = serde_json::from_str("\"streamable_http\"").unwrap();
        assert_eq!(parsed, TransportType::StreamableHttp);
    }
}
