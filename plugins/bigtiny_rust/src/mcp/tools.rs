use serde_json::Value;

use crate::models::mcp::ToolDefinition;

/// Ported from `mcp/tools.py`: byte-based (not char-based) truncation limit.
pub const MAX_TOOL_OUTPUT_BYTES: usize = 100 * 1024;
pub const TRUNCATION_MESSAGE: &str =
    "[Output truncated at 100KB. Use server-specific pagination to retrieve full data.]";

/// Truncate `content` to `MAX_TOOL_OUTPUT_BYTES` UTF-8 *bytes* (matching
/// Python's byte-based limit, not a char-based one). Returns `(content, was_truncated)`.
pub fn truncate_output(content: &str) -> (String, bool) {
    let bytes = content.as_bytes();
    if bytes.len() <= MAX_TOOL_OUTPUT_BYTES {
        return (content.to_string(), false);
    }
    let truncated = String::from_utf8_lossy(&bytes[..MAX_TOOL_OUTPUT_BYTES]).into_owned();
    (format!("{}\n{}", truncated, TRUNCATION_MESSAGE), true)
}

/// Extract text from a `tools/call` JSON-RPC result's `content` array — used
/// by the hand-rolled SSE transport, which deals in raw JSON like the Python
/// reference does. Ported exactly from `_extract_content`: text parts pass
/// through as-is, resource parts are stringified, everything else is dropped.
pub fn extract_content_from_json(content: &Value) -> String {
    let Some(parts) = content.as_array() else {
        return String::new();
    };
    let mut pieces = Vec::new();
    for part in parts {
        if let Some(s) = part.as_str() {
            pieces.push(s.to_string());
            continue;
        }
        match part.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    pieces.push(text.to_string());
                }
            }
            Some("resource") => {
                if let Some(resource) = part.get("resource") {
                    pieces.push(resource.to_string());
                }
            }
            _ => {}
        }
    }
    pieces.join("\n")
}

/// Same extraction rules as `extract_content_from_json`, for rmcp's typed
/// `CallToolResult.content` (used by the stdio/streamable_http paths).
pub fn extract_content_from_rmcp(content: &[rmcp::model::Content]) -> String {
    let mut pieces = Vec::new();
    for part in content {
        match &part.raw {
            rmcp::model::RawContent::Text(t) => pieces.push(t.text.clone()),
            rmcp::model::RawContent::Resource(r) => {
                pieces.push(
                    serde_json::to_value(r)
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                );
            }
            _ => {}
        }
    }
    pieces.join("\n")
}

/// Validate `args` against a tool's JSON Schema (`input_schema`), mirroring
/// Python's `validate_tool_args` — a no-op if the schema is empty/absent, and
/// fails open (treats as valid) if the schema itself is malformed, matching
/// Python's lenient behavior rather than rejecting the call outright.
pub fn validate_tool_args(tool: &ToolDefinition, args: &Value) -> Result<(), String> {
    let is_empty_schema = match &tool.input_schema {
        Value::Null => true,
        Value::Object(m) => m.is_empty(),
        _ => false,
    };
    if is_empty_schema {
        return Ok(());
    }
    match jsonschema::validate(&tool.input_schema, args) {
        Ok(()) => Ok(()),
        Err(e) => {
            let path = e.instance_path.to_string();
            let path = if path.is_empty() {
                "root".to_string()
            } else {
                path
            };
            Err(format!("{path} -> {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn truncate_output_leaves_short_content_untouched() {
        let (content, truncated) = truncate_output("hello");
        assert_eq!(content, "hello");
        assert!(!truncated);
    }

    #[test]
    fn truncate_output_truncates_long_content_by_bytes() {
        let long = "a".repeat(MAX_TOOL_OUTPUT_BYTES + 100);
        let (content, truncated) = truncate_output(&long);
        assert!(truncated);
        assert!(content.contains(TRUNCATION_MESSAGE));
        assert!(content.len() < long.len());
    }

    #[test]
    fn extract_content_from_json_joins_text_and_resource_parts() {
        let content = json!([
            {"type": "text", "text": "hello"},
            {"type": "resource", "resource": {"uri": "file:///x"}},
            {"type": "image", "data": "..."},
            "bare string"
        ]);
        let out = extract_content_from_json(&content);
        assert!(out.contains("hello"));
        assert!(out.contains("file:///x"));
        assert!(out.contains("bare string"));
    }

    #[test]
    fn validate_tool_args_noop_on_empty_schema() {
        let tool = ToolDefinition {
            name: "t".into(),
            description: "".into(),
            input_schema: json!({}),
            server_id: "s".into(),
        };
        assert!(validate_tool_args(&tool, &json!({"anything": 1})).is_ok());
    }

    #[test]
    fn validate_tool_args_rejects_mismatched_args() {
        let tool = ToolDefinition {
            name: "t".into(),
            description: "".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
            server_id: "s".into(),
        };
        assert!(validate_tool_args(&tool, &json!({})).is_err());
        assert!(validate_tool_args(&tool, &json!({"path": "/tmp"})).is_ok());
    }
}
