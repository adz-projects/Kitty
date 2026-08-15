use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::pin::Pin;

use crate::error::ProviderError;
pub use crate::models::provider::{HealthStatus, ModelInfo};

/// Delta chunk emitted by a provider during streaming chat completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delta {
    pub role: String,
    pub content: Option<String>,
    pub reasoning: Option<String>,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub finish_reason: Option<String>,
    pub usage: Option<HashMap<String, i32>>,
    pub error_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(default = "default_type")]
    pub r#type: String,
    pub function: Value,
}

fn default_type() -> String {
    "function".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallChunk {
    pub index: usize,
    pub id: Option<String>,
    pub r#type: Option<String>,
    pub function: Option<Value>,
}

/// Sampling parameters for one `chat_completion` call, already resolved
/// (configured value, or the model-aware default for a self-hosted provider
/// — see `provider::sampling::defaults_for`) so providers just serialize
/// whatever is `Some`.
///
/// Collapsed into one struct rather than more positional arguments: without
/// this, `chat_completion` (already carrying 7 parameters behind an
/// `#[allow(clippy::too_many_arguments)]`) would need 6 more for the fields
/// this fixes — `temperature`/`top_p` existed but were dead (never plumbed
/// past `agent::loop_`, and Anthropic's `top_p` was `_top_p`, unread), which
/// combined with llama-server's own defaults (repetition control fully
/// disabled) is what let a quantized Qwen model stream an unbounded
/// repetition loop.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SamplingParams {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    /// llama.cpp/Ollama extension. Only ever `Some` for a `provider_type` of
    /// `ollama`/`custom_openai` (see `sampling::defaults_for`) — hosted
    /// OpenAI-compatible endpoints reject it, and Anthropic has no
    /// equivalent.
    pub top_k: Option<i32>,
    /// llama.cpp/Ollama extension; same scoping as `top_k`.
    pub min_p: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub frequency_penalty: Option<f64>,
    pub max_tokens: Option<i32>,
    /// Requested reasoning effort for this turn, or `None` for "don't ask".
    ///
    /// Unlike every other field here — each of which the provider just
    /// serializes as-is — effort is *translated per dialect*: a flat
    /// `reasoning_effort` string on OpenAI, a nested `reasoning` object on
    /// OpenRouter, a `thinking` token budget on Anthropic, and nothing at all
    /// on a self-hosted/llama.cpp endpoint (which has no such parameter). It
    /// rides on `SamplingParams` for the same reason `max_tokens` does — it's a
    /// per-turn budget knob, not a style choice — which is also why presets
    /// leave it `None` (see `presets.rs`): a preset is about creativity, not
    /// reasoning. The agent loop sets it after merging presets/floors, so
    /// `merge`'s `or()` semantics never combine two efforts.
    pub effort: Option<Effort>,
}

/// A requested reasoning-effort level, provider-agnostic. Mapped to each
/// provider's own dialect at the wire boundary (see `SamplingParams::effort`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    /// Explicitly ask the model *not* to spend reasoning tokens. Expressible
    /// on OpenRouter (`{"enabled": false}`) and by omission on Anthropic;
    /// OpenAI's o-series has no "off", so this degrades to "send nothing".
    Off,
    Low,
    Medium,
    High,
}

impl Effort {
    /// Parse the wire string Kitty persists in session config
    /// (`thinking_effort`). Unknown/empty → `None`, i.e. "no effort requested",
    /// never a silent default to some level.
    pub fn from_wire(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Effort::Off),
            "low" => Some(Effort::Low),
            "medium" => Some(Effort::Medium),
            "high" => Some(Effort::High),
            _ => None,
        }
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Unique identifier for this provider instance.
    fn provider_id(&self) -> &str;

    /// Resolve model from: caller override > config > DEFAULT_MODEL.
    fn resolve_model(&self, override_model: Option<&str>) -> String;

    /// Stream chat completion as an async iterator of Delta chunks.
    ///
    /// `id_slot`, when `Some`, is forwarded to llama-server-compatible
    /// endpoints to pin a session to a fixed inference slot (see
    /// `prompt_determinism.md`) — providers that don't understand the field
    /// (real Anthropic/OpenAI) either ignore it or never receive `Some` in
    /// the first place, since it's only computed when slot pinning is
    /// explicitly configured.
    async fn chat_completion(
        &self,
        messages: Vec<Value>,
        tools: Option<Vec<Value>>,
        sampling: SamplingParams,
        model: Option<String>,
        id_slot: Option<i32>,
    ) -> Result<Pin<Box<dyn Stream<Item = Delta> + Send>>, ProviderError>;

    /// Discover available models from the provider's API.
    async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;

    /// Check provider health.
    async fn check_health(&self) -> HealthStatus;

    /// Whether this provider's wire protocol tolerates a trailing partial
    /// `role: "assistant"` message as the last entry in `messages` and
    /// continues generation from it, rather than erroring or starting a
    /// fresh turn. Gates thought-seeding (`agent::loop_::pathway_recall`) --
    /// seeding a `<think>` prefill into a provider that doesn't actually
    /// honor it would either be silently ignored (wasted context) or, worse,
    /// leak the raw seed framing into the visible answer. Default `false`;
    /// override only where verified or explicitly opted into (see each
    /// implementor).
    fn supports_assistant_prefill(&self) -> bool {
        false
    }

    /// Whether this provider can be given tool definitions at all.
    ///
    /// Default `true` — every HTTP provider here speaks a tool-calling
    /// dialect. The in-process llama.cpp engine is the exception, and used to
    /// express that by silently discarding whatever tools it was handed and
    /// logging a `warn!` nobody reads: sessions on a local model got a model
    /// that could not act, with no signal anywhere the user could see.
    /// Declaring the capability instead lets the agent loop say so once, in
    /// the stream, and skip sending tools it knows will be dropped.
    fn supports_tools(&self) -> bool {
        true
    }
}

/// Cap a raw provider error body before it's embedded in a user-facing
/// message. Provider error bodies can be huge and can echo request content
/// (prompts, tool args — and on some self-hosted backends, auth material)
/// back at us; the full text stays in `raw_message` for diagnostics.
fn sanitize_body_for_user(body: &str) -> String {
    const MAX_USER_BODY_CHARS: usize = 300;
    let trimmed = body.trim();
    if trimmed.chars().count() <= MAX_USER_BODY_CHARS {
        trimmed.to_string()
    } else {
        format!(
            "{}…[truncated]",
            trimmed.chars().take(MAX_USER_BODY_CHARS).collect::<String>()
        )
    }
}

/// Classify an HTTP error into a structured provider error.
pub fn classify_provider_error(status_code: u16, body: &str) -> ProviderError {
    let lower = body.to_lowercase();

    if lower.contains("insufficient_quota")
        || lower.contains("billing")
        || lower.contains("quota")
        || lower.contains("credit")
        || status_code == 402
    {
        return ProviderError::InsufficientCredits {
            user_message: "Insufficient credits. Check your provider billing.".into(),
            raw_message: body.into(),
            http_status: status_code as i32,
        };
    }

    if lower.contains("context_length_exceeded")
        || lower.contains("context") && lower.contains("maximum")
        || lower.contains("too long")
    {
        return ProviderError::ContextExceeded {
            user_message: "Context window exceeded. Consider compacting the session.".into(),
            raw_message: body.into(),
            http_status: status_code as i32,
        };
    }

    ProviderError::Other {
        user_message: format!("Provider error: {}", sanitize_body_for_user(body)),
        raw_message: body.into(),
        http_status: status_code as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_insufficient_credits() {
        let err = classify_provider_error(402, "Billing issue");
        match err {
            ProviderError::InsufficientCredits { .. } => {}
            other => panic!("Expected InsufficientCredits, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_context_exceeded() {
        let err = classify_provider_error(400, "context_length_exceeded");
        match err {
            ProviderError::ContextExceeded { .. } => {}
            other => panic!("Expected ContextExceeded, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_other() {
        let err = classify_provider_error(500, "Internal error");
        match err {
            ProviderError::Other { .. } => {}
            other => panic!("Expected Other, got {:?}", other),
        }
    }

    /// A huge provider error body must not land verbatim in the user-facing
    /// message (it can echo request content back); the full text stays in
    /// `raw_message` for diagnostics.
    #[test]
    fn test_classify_other_truncates_the_user_facing_body() {
        let huge = "x".repeat(10_000);
        let err = classify_provider_error(500, &huge);
        match err {
            ProviderError::Other {
                user_message,
                raw_message,
                ..
            } => {
                assert!(user_message.len() < 400, "user message must be capped: {user_message:?}");
                assert!(user_message.contains("truncated"));
                assert_eq!(raw_message, huge, "raw_message keeps the full body");
            }
            other => panic!("Expected Other, got {:?}", other),
        }
    }
}
