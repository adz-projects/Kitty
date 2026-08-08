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
        user_message: format!("Provider error: {}", body),
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
}
