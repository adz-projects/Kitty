use serde::{Deserialize, Serialize};

/// Every SSE event type the daemon emits. All but `ScheduleRun` come from the
/// agent loop on a turn's own stream; `ScheduleRun` exists only on the per-app
/// event stream (`GET /api/apps/me/events`).
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
    /// A scheduled task's run started, finished or failed. `schedule_id`
    /// names the schedule, `session_id` the session the run happened in, and
    /// `content` the outcome (`started` | `finished` | `failed` |
    /// `denied_by_timeout`).
    ScheduleRun,
}

/// Wire-format event pushed over SSE from the agent loop to the frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SSEEvent {
    #[serde(rename = "type")]
    pub event_type: SSEEventType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// The id of the tool call this frame belongs to, on `ToolStart`/
    /// `ToolFinish` — a step's tool calls run concurrently, so arrival order
    /// cannot pair a finish with its start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_args: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_id: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_last: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    // `default_true`, not `default`: this field is omitted from the wire
    // when it is `true`, so a missing value means true. Deriving `false`
    // here would turn every ordinary recoverable error into a fatal one
    // on the client side.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub recoverable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttfb_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_ms: Option<f64>,
    /// Generation speed for this LLM call, computed daemon-side — see
    /// `agent::types::TimingResult::finalize_rate` for why the client must
    /// not derive it itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_second: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<i64>,
    /// On `ToolFinish`: whether the tool call failed (the tool reported an
    /// error, or it was denied). Absent on older daemons, where a client had
    /// to guess from the result text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    /// On `ModelFailover`: why the model or provider changed --
    /// `pinned_unavailable` (the chat's pinned provider is gone),
    /// `no_tool_support` (the provider cannot call tools, so this turn runs
    /// without them), or `error_switch` (the provider failed mid-turn and
    /// another took over). `provider_id`/`model` name what is now answering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// On `ModelFailover`: the provider that was replaced, when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_provider_id: Option<String>,
    /// On `ScheduleRun`: the schedule this run belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_id: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !b
}

fn is_true(b: &bool) -> bool {
    *b
}

/// The default for `recoverable`, which is omitted from the wire when true.
fn default_true() -> bool {
    true
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
            tool_call_id: None,
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
            is_error: None,
            reason: None,
            from_provider_id: None,
            schedule_id: None,
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

#[cfg(test)]
mod roundtrip_tests {
    use super::*;

    #[test]
    fn an_event_deserializes_its_own_serialized_form() {
        // The contract this crate exists to guarantee. `skip_serializing_if`
        // without `default` omits a field on the way out and then *requires*
        // it on the way in, so the daemon's own output would not parse in a
        // client -- which is exactly the drift the shared crate prevents.
        for event in [
            SSEEvent::content("hello"),
            SSEEvent::reasoning("thinking"),
            SSEEvent::stop("done", None),
            SSEEvent {
                event_type: SSEEventType::ToolStart,
                tool_name: Some("read_file".into()),
                ..Default::default()
            },
        ] {
            let wire = serialize_sse(&event);
            let json = wire.trim_start_matches("data: ").trim();
            let back: SSEEvent = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("{:?} did not round-trip: {e}\n{json}", event.event_type));
            assert_eq!(back.event_type, event.event_type);
            assert_eq!(back.content, event.content);
            assert_eq!(back.is_last, event.is_last);
        }
    }

    #[test]
    fn the_v2_1_fields_round_trip_and_stay_off_the_wire_when_unset() {
        let failover = SSEEvent {
            event_type: SSEEventType::ModelFailover,
            reason: Some("error_switch".into()),
            provider_id: Some("b".into()),
            from_provider_id: Some("a".into()),
            ..Default::default()
        };
        let finish = SSEEvent {
            event_type: SSEEventType::ToolFinish,
            is_error: Some(true),
            ..Default::default()
        };
        let run = SSEEvent {
            event_type: SSEEventType::ScheduleRun,
            schedule_id: Some("s1".into()),
            content: Some("finished".into()),
            ..Default::default()
        };
        for event in [failover, finish, run] {
            let wire = serialize_sse(&event);
            let json = wire.trim_start_matches("data: ").trim();
            let back: SSEEvent = serde_json::from_str(json).unwrap();
            assert_eq!(back.event_type, event.event_type);
            assert_eq!(back.reason, event.reason);
            assert_eq!(back.from_provider_id, event.from_provider_id);
            assert_eq!(back.is_error, event.is_error);
            assert_eq!(back.schedule_id, event.schedule_id);
        }
        // An ordinary delta carries none of them.
        let plain = serialize_sse(&SSEEvent::content("x"));
        for field in ["is_error", "reason", "from_provider_id", "schedule_id"] {
            assert!(!plain.contains(field), "{field} leaked onto a plain delta: {plain}");
        }
    }
}
