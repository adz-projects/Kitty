use std::time::Duration;

use serde_json::{json, Value};

use crate::config::SummarizerConfig;
use crate::error::SummarizerError;

/// `adaptive_pathway::StructuredChat` impl over the shared summarizer client
/// (LFM2.5-like 0.6/0.8b model) — the engine's learned `structured_chat`
/// extraction. This is the trait-inversion seam that lets adaptive_pathway
/// depend on a plain interface while bigtiny_rust provides the real client.
#[async_trait::async_trait]
impl adaptive_pathway::traits::StructuredChat for SummarizerClient {
    async fn structured_chat(
        &self,
        messages: Vec<Value>,
        schema: &Value,
    ) -> Result<Value, String> {
        self.structured_chat(messages, schema)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Talks to Ollama's *native* `/api/chat` endpoint (not the OpenAI-compatible
/// `/v1/chat/completions` path providers use) with a JSON-schema-constrained
/// `format` field, for the compaction summarizer's structured memory-slot
/// extraction. Ports `plugins/bigtiny/bigtiny/providers/summarizer_client.py`
/// — deliberately its own small client rather than going through the
/// `Provider`/`ProviderRouter` abstraction, since the wire format and even
/// the base API (Ollama-native vs. OpenAI-compatible) differ from every other
/// provider call in this codebase.
pub struct SummarizerClient {
    client: reqwest::Client,
    config: SummarizerConfig,
}

impl SummarizerClient {
    pub fn new(config: SummarizerConfig) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs_f64(config.timeout_s))
            .build()
            .unwrap_or_default();
        Self { client, config }
    }

    /// Request a structured (JSON-schema-constrained) chat completion. Never
    /// panics; any failure (connection, non-2xx, missing/empty content,
    /// invalid JSON) becomes a `SummarizerError` for the caller to treat as
    /// "compaction pass skipped this time," never a hard failure.
    pub async fn structured_chat(
        &self,
        messages: Vec<Value>,
        json_schema: &Value,
    ) -> Result<Value, SummarizerError> {
        let body = json!({
            "model": self.config.model,
            "messages": messages,
            "stream": false,
            "think": false,
            "format": json_schema,
            "keep_alive": self.config.keep_alive,
            "options": {"temperature": self.config.temperature},
        });

        let url = format!("{}/api/chat", self.config.base_url.trim_end_matches('/'));

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| SummarizerError::Request(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(SummarizerError::Http {
                status: status.as_u16(),
                body: body_text,
            });
        }

        let payload: Value = resp
            .json()
            .await
            .map_err(|e| SummarizerError::Request(e.to_string()))?;

        let content = payload
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .filter(|s| !s.is_empty())
            .ok_or(SummarizerError::EmptyContent)?;

        serde_json::from_str(content).map_err(|e| SummarizerError::InvalidJson(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(base_url: String) -> SummarizerConfig {
        SummarizerConfig {
            enabled: true,
            model: "qwen3.5:0.8b".into(),
            base_url,
            keep_alive: "5m".into(),
            temperature: 0.1,
            timeout_s: 5.0,
            reserve_exchanges: 3,
            max_slot_items: 20,
        }
    }

    #[tokio::test]
    async fn structured_chat_parses_happy_path() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/api/chat")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"message": {"role": "assistant", "content": "{\"current_state\": \"testing\"}"}}"#,
            )
            .create_async()
            .await;

        let client = SummarizerClient::new(test_config(server.url()));
        let result = client
            .structured_chat(vec![json!({"role": "user", "content": "hi"})], &json!({}))
            .await
            .unwrap();
        assert_eq!(result["current_state"], "testing");
    }

    #[tokio::test]
    async fn structured_chat_errors_on_non_2xx() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/api/chat")
            .with_status(500)
            .with_body("internal error")
            .create_async()
            .await;

        let client = SummarizerClient::new(test_config(server.url()));
        let result = client.structured_chat(vec![], &json!({})).await;
        assert!(matches!(
            result,
            Err(SummarizerError::Http { status: 500, .. })
        ));
    }

    #[tokio::test]
    async fn structured_chat_errors_on_empty_content() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/api/chat")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"message": {"role": "assistant", "content": ""}}"#)
            .create_async()
            .await;

        let client = SummarizerClient::new(test_config(server.url()));
        let result = client.structured_chat(vec![], &json!({})).await;
        assert!(matches!(result, Err(SummarizerError::EmptyContent)));
    }

    #[tokio::test]
    async fn structured_chat_errors_on_invalid_json_content() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/api/chat")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"message": {"role": "assistant", "content": "not json"}}"#)
            .create_async()
            .await;

        let client = SummarizerClient::new(test_config(server.url()));
        let result = client.structured_chat(vec![], &json!({})).await;
        assert!(matches!(result, Err(SummarizerError::InvalidJson(_))));
    }

    #[tokio::test]
    async fn structured_chat_errors_on_connection_failure() {
        let client = SummarizerClient::new(test_config("http://127.0.0.1:1".into()));
        let result = client.structured_chat(vec![], &json!({})).await;
        assert!(matches!(result, Err(SummarizerError::Request(_))));
    }
}
