use serde::{Deserialize, Serialize};

/// All 15 SSE event types emitted by the agent loop to the frontend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SSEEventType {
    LlmDelta,
    ReasoningDelta,
    LlmStop,
    ToolStart,
    ToolFinish,
    HitlPause,
    HitlResolved,
    Error,
    ModelFailover,
    SubagentStatus,
    SessionStatus,
    SessionTitle,
    Compaction,
    ProviderError,
    LlmTiming,
}

/// Wire-format event pushed over SSE from the agent loop to the frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SSEEvent {
    #[serde(rename = "type")]
    pub event_type: SSEEventType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_args: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_id: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub is_last: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(skip_serializing_if = "is_true")]
    pub recoverable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttfb_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_ms: Option<f64>,
    /// Generation speed for this LLM call, computed daemon-side — see
    /// `agent::types::TimingResult::finalize_rate` for why the client must
    /// not derive it itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_per_second: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<i64>,
}

fn is_false(b: &bool) -> bool {
    !b
}

fn is_true(b: &bool) -> bool {
    *b
}

impl SSEEvent {
    pub fn content(content: impl Into<String>) -> Self {
        Self {
            event_type: SSEEventType::LlmDelta,
            content: Some(content.into()),
            ..Default::default()
        }
    }

    pub fn reasoning(content: impl Into<String>) -> Self {
        Self {
            event_type: SSEEventType::ReasoningDelta,
            content: Some(content.into()),
            ..Default::default()
        }
    }

    pub fn stop(finish_reason: impl Into<String>, usage: Option<serde_json::Value>) -> Self {
        Self {
            event_type: SSEEventType::LlmStop,
            content: Some(finish_reason.into()),
            usage,
            is_last: true,
            ..Default::default()
        }
    }
}

impl Default for SSEEvent {
    fn default() -> Self {
        Self {
            event_type: SSEEventType::LlmDelta,
            content: None,
            tool_name: None,
            tool_args: None,
            tool_result: None,
            duration_ms: None,
            session_id: None,
            usage: None,
            action_id: None,
            is_last: false,
            error_code: None,
            error_message: None,
            recoverable: true,
            error_type: None,
            ttfb_ms: None,
            ttft_ms: None,
            generation_ms: None,
            tokens_per_second: None,
            provider_id: None,
            model: None,
            total_tokens: None,
        }
    }
}

/// Serialize an SSEEvent to SSE wire format: `data: {json}\n\n`.
pub fn serialize_sse(event: &SSEEvent) -> String {
    let payload = serde_json::to_string(event).expect("SSEEvent is always serializable");
    format!("data: {payload}\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sse_event_content() {
        let e = SSEEvent::content("hello");
        assert_eq!(e.event_type, SSEEventType::LlmDelta);
        assert_eq!(e.content, Some("hello".into()));
    }

    #[test]
    fn test_serialize_sse_format() {
        let e = SSEEvent::content("hi");
        let s = serialize_sse(&e);
        assert!(s.starts_with("data: "));
        assert!(s.ends_with("\n\n"));
    }

    #[test]
    fn test_sse_event_stop() {
        let e = SSEEvent::stop("done", None);
        assert_eq!(e.event_type, SSEEventType::LlmStop);
        assert!(e.is_last);
    }
}
