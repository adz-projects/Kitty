use async_trait::async_trait;
use futures::Stream;
use serde_json::Value;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use super::base::{
    classify_provider_error, Delta, HealthStatus, ModelInfo, Provider, SamplingParams,
};
use crate::config::{CacheConfig, ProviderConfig};
use crate::error::ProviderError;
use crate::network::{maybe_direct_url, TailscaleClient};

pub struct AnthropicProvider {
    pub provider_id: String,
    pub config: ProviderConfig,
    client: reqwest::Client,
    tailscale: Arc<TailscaleClient>,
    cache: CacheConfig,
    /// SSE idle-read timeout — see `OpenAICompatibleProvider::idle_timeout`.
    idle_timeout: Duration,
}

impl AnthropicProvider {
    pub const DEFAULT_MODEL: &'static str = "claude-sonnet-4-20250514";

    pub fn new(
        provider_id: &str,
        config: ProviderConfig,
        tailscale: Arc<TailscaleClient>,
        cache: CacheConfig,
    ) -> Self {
        let idle_timeout = config.idle_timeout();
        Self {
            provider_id: provider_id.into(),
            client: reqwest::Client::new(),
            config,
            tailscale,
            cache,
            idle_timeout,
        }
    }

    /// See `OpenAICompatibleProvider::send_preferring_direct` — same
    /// Tailscale direct-address-first, tunnel-fallback behavior.
    async fn send_preferring_direct(
        &self,
        url: &str,
        body: &Value,
    ) -> Result<reqwest::Response, reqwest::Error> {
        if let Some(direct_url) = maybe_direct_url(&self.tailscale, url).await {
            let direct = self
                .client
                .post(&direct_url)
                .header("x-api-key", &self.config.api_key)
                .header("anthropic-version", "2023-06-01")
                .timeout(crate::network::DIRECT_CONNECT_TIMEOUT)
                .json(body)
                .send()
                .await;
            if let Ok(resp) = direct {
                return Ok(resp);
            }
        }
        self.client
            .post(url)
            .header("x-api-key", &self.config.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(body)
            .send()
            .await
    }

    fn group_tool_results(messages: &[Value]) -> Vec<Value> {
        let mut result = Vec::new();
        let mut tool_accumulator: Vec<Value> = Vec::new();

        for msg in messages {
            if msg["role"] == "tool" {
                let mut block = serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": msg["tool_call_id"].as_str().unwrap_or(""),
                });
                if let Some(content) = msg["content"].as_str() {
                    block["content"] = content.into();
                } else {
                    block["content"] = Value::Null;
                }
                tool_accumulator.push(block);
            } else {
                if !tool_accumulator.is_empty() {
                    result.push(serde_json::json!({
                        "role": "user",
                        "content": tool_accumulator.clone(),
                    }));
                    tool_accumulator.clear();
                }
                result.push(msg.clone());
            }
        }

        if !tool_accumulator.is_empty() {
            result.push(serde_json::json!({
                "role": "user",
                "content": tool_accumulator,
            }));
        }

        result
    }

    fn convert_tool_calls(msg: &Value) -> Value {
        if let Some(calls) = msg["tool_calls"].as_array() {
            let content: Vec<Value> = calls
                .iter()
                .map(|tc| {
                    serde_json::json!({
                        "type": "tool_use",
                        "id": tc["id"].as_str().unwrap_or(""),
                        "name": tc["function"]["name"].as_str().unwrap_or(""),
                        "input": tc["function"]["arguments"].clone(),
                    })
                })
                .collect();
            serde_json::json!({
                "role": "assistant",
                "content": content,
            })
        } else {
            msg.clone()
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn resolve_model(&self, override_model: Option<&str>) -> String {
        if let Some(m) = override_model {
            return m.into();
        }
        if !self.config.model.is_empty() {
            return self.config.model.clone();
        }
        Self::DEFAULT_MODEL.into()
    }

    /// Anthropic's Messages API natively treats a trailing `role:
    /// "assistant"` message as a prefill to continue generation from --
    /// documented protocol behavior, not a guess, so this is unconditional
    /// (no config opt-in needed, unlike `OpenAICompatibleProvider`).
    fn supports_assistant_prefill(&self) -> bool {
        true
    }

    async fn chat_completion(
        &self,
        messages: Vec<Value>,
        tools: Option<Vec<Value>>,
        sampling: SamplingParams,
        model: Option<String>,
        id_slot: Option<i32>,
    ) -> Result<Pin<Box<dyn Stream<Item = Delta> + Send>>, ProviderError> {
        let model = self.resolve_model(model.as_deref());
        let url = format!("{}/v1/messages", self.config.base_url);

        let (system, non_system): (Vec<Value>, Vec<Value>) = messages
            .iter()
            .map(Self::convert_tool_calls)
            .partition(|m| m["role"] == "system");

        let grouped = Self::group_tool_results(&non_system);

        let mut body = serde_json::json!({
            "model": model,
            "messages": grouped,
            "max_tokens": sampling.max_tokens.unwrap_or(4096),
            "stream": true,
        });

        if !system.is_empty() {
            // Context building layers up to 5 separate system messages
            // (persona, session override, writable-dir hint, anchored first
            // message, consolidated memory — see
            // `agent/context/builder.rs`). Anthropic's API takes a single
            // `system` string, so all of them need to survive here, not
            // just the first — keeping only `system[0]` silently dropped
            // every layer after the base persona on every Anthropic turn.
            let joined = system
                .iter()
                .filter_map(|m| m["content"].as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            body["system"] = if self.cache.anthropic_cache_control {
                // A single `cache_control` breakpoint on the (whole,
                // already-joined) system block caches everything upstream of
                // it — the conversation turns that follow are downstream of
                // this prefix, so one marker is sufficient; no per-layer
                // breakpoints needed.
                serde_json::json!([{
                    "type": "text",
                    "text": joined,
                    "cache_control": {"type": "ephemeral"},
                }])
            } else {
                Value::String(joined)
            };
        }
        if let Some(t) = sampling.temperature {
            body["temperature"] = t.into();
        }
        // Anthropic's Messages API takes temperature XOR top_p; sending both
        // is a 400. Prefer temperature when a caller (unusually) sets both.
        if sampling.temperature.is_none() {
            if let Some(p) = sampling.top_p {
                body["top_p"] = p.into();
            }
        }
        if let Some(t) = tools {
            body["tools"] = t.into();
        }
        // `id_slot` is a llama.cpp/vLLM-only field (`parallel_slots`, self-hosted
        // providers only, see openai_compat.rs's dialect gate) — Anthropic's
        // real API has no such field, so it's never written onto this wire body.
        let _ = id_slot;

        let resp = self
            .send_preferring_direct(&url, &body)
            .await
            .map_err(|e| ProviderError::Request {
                user_message: format!("Failed to connect to Anthropic: {}", e),
                raw_message: e.to_string(),
                http_status: 0,
            })?;

        let status_code = resp.status().as_u16();
        if !resp.status().is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(classify_provider_error(status_code, &body_text));
        }

        let stream = resp.bytes_stream();
        let deltas = parse_anthropic_sse(stream, self.idle_timeout);
        Ok(Box::pin(deltas))
    }

    async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let url = format!("{}/v1/models", self.config.base_url);
        let resp = self
            .client
            .get(&url)
            .header("x-api-key", &self.config.api_key)
            .send()
            .await
            .map_err(|e| ProviderError::Request {
                user_message: format!("Failed to discover models: {}", e),
                raw_message: e.to_string(),
                http_status: 0,
            })?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(classify_provider_error(0, &body));
        }

        let data: Value = resp.json().await.map_err(|e| ProviderError::Other {
            user_message: format!("Failed to parse models response: {}", e),
            raw_message: e.to_string(),
            http_status: 0,
        })?;

        let models: Vec<ModelInfo> = data["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        Some(ModelInfo {
                            id: m["id"].as_str()?.to_string(),
                            name: m["name"].as_str().map(|s| s.into()),
                            provider_id: Some(self.provider_id.clone()),
                            context_length: m["max_model_len"].as_i64().map(|v| v as i32),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(models)
    }

    async fn check_health(&self) -> HealthStatus {
        let url = format!("{}/v1/models", self.config.base_url);
        let start = std::time::Instant::now();

        // A per-request timeout, not the shared client's default — `self.client`
        // is also used for chat completions, which can legitimately run far
        // longer than a health probe should ever be allowed to block for.
        match self
            .client
            .get(&url)
            .header("x-api-key", &self.config.api_key)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let latency = start.elapsed().as_secs_f64() * 1000.0;
                HealthStatus {
                    status: "healthy".into(),
                    latency_ms: Some(latency),
                    error: None,
                }
            }
            Ok(resp) => HealthStatus {
                status: "unhealthy".into(),
                latency_ms: Some(start.elapsed().as_secs_f64() * 1000.0),
                error: Some(format!("HTTP {}", resp.status().as_u16())),
            },
            Err(e) => HealthStatus {
                status: "unhealthy".into(),
                latency_ms: None,
                error: Some(e.to_string()),
            },
        }
    }
}

/// Build a `Delta.usage` map from an Anthropic `usage` object (found at
/// `message.usage` on `message_start`, and again — cumulatively updated,
/// output tokens only in practice, but read defensively — at `usage` on
/// `message_delta`). Field names are normalized to the same keys
/// `openai_compat.rs` uses (`cache_read_tokens`/`cache_creation_tokens`) so
/// downstream code never needs to know which provider produced them.
///
/// Anthropic's `input_tokens` deliberately *excludes* whatever was served
/// from cache — OpenAI-style `prompt_tokens` does the opposite, it already
/// counts cached tokens as part of the total. Left as-is, the same
/// `input_tokens` key would mean two different things depending on which
/// provider produced it, silently corrupting both the "Tokens: N in" display
/// and any cache-hit-rate calculation derived from it (`cache_read_tokens /
/// input_tokens` would double an Anthropic turn's true prompt size out from
/// under it). Normalized here instead: `input_tokens` always means *total*
/// prompt size, cache included, matching OpenAI's semantics — a fresh-only
/// count isn't exposed separately since nothing downstream needs it.
///
/// Returns `None` rather than an empty map when the object has nothing
/// usable, so a Delta with no real usage data doesn't spuriously participate
/// in the per-turn merge in `agent/loop_.rs::process_stream`.
fn usage_map_from_anthropic(usage: &Value) -> Option<HashMap<String, i32>> {
    let mut map = HashMap::new();
    let fresh_input = usage["input_tokens"].as_i64();
    let cache_creation = usage["cache_creation_input_tokens"].as_i64();
    let cache_read = usage["cache_read_input_tokens"].as_i64();
    if fresh_input.is_some() || cache_creation.is_some() || cache_read.is_some() {
        let total_input =
            fresh_input.unwrap_or(0) + cache_creation.unwrap_or(0) + cache_read.unwrap_or(0);
        map.insert("input_tokens".to_string(), total_input as i32);
    }
    if let Some(v) = usage["output_tokens"].as_i64() {
        map.insert("output_tokens".to_string(), v as i32);
    }
    if let Some(v) = cache_creation {
        map.insert("cache_creation_tokens".to_string(), v as i32);
    }
    if let Some(v) = cache_read {
        map.insert("cache_read_tokens".to_string(), v as i32);
    }
    if map.is_empty() {
        None
    } else {
        Some(map)
    }
}

/// Parse Anthropic SSE stream into Delta chunks.
///
/// Buffers a trailing partial line across polls (`buf`) and queues every Delta
/// produced while parsing one chunk (`pending`) since a single chunk routinely
/// contains multiple `data:` frames — returning after the first one silently
/// dropped the rest. `Poll::Pending` from the inner stream is propagated as-is,
/// never conflated with end-of-stream.
/// Accumulates one streamed `tool_use` content block. Anthropic sends the
/// tool's `id`/`name` once, in `content_block_start`; the arguments arrive
/// incrementally afterward as `input_json_delta` fragments.
#[derive(Default)]
struct PendingToolCall {
    id: Option<String>,
    name: Option<String>,
    input_json: String,
}

type RawBytesStream = Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>;

struct AnthropicSSEStream {
    /// `tokio_stream::adapters::Timeout` used as an idle-read timeout — see
    /// `parse_openai_sse` in openai_compat.rs for the rationale.
    inner: Pin<Box<tokio_stream::adapters::Timeout<RawBytesStream>>>,
    tool_input_buf: HashMap<usize, PendingToolCall>,
    pending_tool_calls: Vec<super::base::ToolCall>,
    /// Raw bytes between newlines, decoded to UTF-8 only at complete line
    /// boundaries (see openai_compat.rs `buf`).
    buf: Vec<u8>,
    pending: std::collections::VecDeque<Delta>,
    done: bool,
}

fn parse_anthropic_sse(
    stream: impl Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    idle_timeout: Duration,
) -> AnthropicSSEStream {
    use tokio_stream::StreamExt as _;
    let inner = Box::pin(stream)
        as Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>;
    let inner = inner.timeout(idle_timeout);
    AnthropicSSEStream {
        inner: Box::pin(inner),
        tool_input_buf: HashMap::new(),
        pending_tool_calls: Vec::new(),
        buf: Vec::new(),
        pending: std::collections::VecDeque::new(),
        done: false,
    }
}

impl AnthropicSSEStream {
    /// Process one complete SSE line, pushing any resulting Delta onto `pending`.
    /// Returns true if this line signalled stream completion (`message_stop`).
    fn process_line(&mut self, line: &str) -> bool {
        if line.starts_with("event: ") {
            return false;
        }
        let Some(data) = line.strip_prefix("data: ") else {
            return false;
        };
        let json: Value = match serde_json::from_str(data) {
            Ok(j) => j,
            Err(_) => return false,
        };
        let event_type = json["type"].as_str().unwrap_or("");

        match event_type {
            "message_start" => {
                if let Some(role) = json["message"]["role"].as_str() {
                    // `message.usage` carries prompt-cache stats up front —
                    // `input_tokens` here already *excludes* whatever was
                    // served from cache (Anthropic docs), so the three
                    // together (this input_tokens + cache_creation +
                    // cache_read) sum to the full prompt token count.
                    // `output_tokens` also appears here (a small placeholder,
                    // updated for real in `message_delta` below) — read it
                    // too so a turn that errors before any `message_delta`
                    // still reports *something* instead of nothing.
                    let msg_usage = &json["message"]["usage"];
                    let usage = usage_map_from_anthropic(msg_usage);
                    self.pending.push_back(Delta {
                        role: role.into(),
                        content: None,
                        reasoning: None,
                        tool_calls: None,
                        finish_reason: None,
                        usage,
                        error_type: None,
                    });
                }
            }
            "content_block_start" => {
                let block = &json["content_block"];
                if block["type"] == "tool_use" {
                    let idx = json["index"].as_u64().unwrap_or(0) as usize;
                    // `block["name"]` was never captured before — every
                    // tool call ended up with `function` set to just the
                    // parsed arguments object, no `name` key anywhere, so
                    // every tool executed as an unnamed/"unknown" tool.
                    self.tool_input_buf.insert(
                        idx,
                        PendingToolCall {
                            id: block["id"].as_str().map(|s| s.into()),
                            name: block["name"].as_str().map(|s| s.into()),
                            input_json: String::new(),
                        },
                    );
                }
            }
            "content_block_delta" => {
                let delta = &json["delta"];
                if delta["type"] == "text_delta" {
                    if let Some(text) = delta["text"].as_str() {
                        self.pending.push_back(Delta {
                            role: "assistant".into(),
                            content: Some(text.into()),
                            reasoning: None,
                            tool_calls: None,
                            finish_reason: None,
                            usage: None,
                            error_type: None,
                        });
                    }
                } else if delta["type"] == "thinking_delta" {
                    if let Some(t) = delta["thinking"].as_str() {
                        self.pending.push_back(Delta {
                            role: "assistant".into(),
                            content: None,
                            reasoning: Some(t.into()),
                            tool_calls: None,
                            finish_reason: None,
                            usage: None,
                            error_type: None,
                        });
                    }
                } else if delta["type"] == "input_json_delta" {
                    if let Some(partial) = delta["partial_json"].as_str() {
                        let idx = json["index"].as_u64().unwrap_or(0) as usize;
                        if let Some(entry) = self.tool_input_buf.get_mut(&idx) {
                            entry.input_json.push_str(partial);
                        }
                    }
                }
            }
            "content_block_stop" => {
                let idx = json["index"].as_u64().unwrap_or(0) as usize;
                if let Some(entry) = self.tool_input_buf.remove(&idx) {
                    // `function` must carry both `name` and `arguments` —
                    // the agent loop reads `tc.function.get("name")` /
                    // `.get("arguments")`. Previously `function` was set to
                    // just the parsed input object with no `name` key at
                    // all, so every tool call executed nameless/"unknown".
                    let arguments: Value = if entry.input_json.is_empty() {
                        serde_json::json!({})
                    } else {
                        serde_json::from_str(&entry.input_json)
                            .unwrap_or_else(|_| serde_json::json!({}))
                    };
                    let function = serde_json::json!({
                        "name": entry.name.unwrap_or_default(),
                        "arguments": arguments,
                    });
                    self.pending_tool_calls.push(super::base::ToolCall {
                        id: entry.id.unwrap_or_default(),
                        r#type: "function".into(),
                        function,
                    });
                }
            }
            "message_delta" => {
                let stop = json["delta"]["stop_reason"].as_str().map(|s| s.into());
                let tool_calls = if !self.pending_tool_calls.is_empty() {
                    Some(self.pending_tool_calls.drain(..).collect())
                } else {
                    None
                };
                // The final, real `output_tokens` count lands here (message_start's
                // was just a placeholder); cache fields are read too in case a
                // future API version repeats them here, but in practice they
                // only appear on message_start.
                let usage = usage_map_from_anthropic(&json["usage"]);
                self.pending.push_back(Delta {
                    role: "assistant".into(),
                    content: None,
                    reasoning: None,
                    tool_calls,
                    finish_reason: stop,
                    usage,
                    error_type: None,
                });
            }
            "message_stop" => {
                return true;
            }
            _ => {}
        }
        false
    }
}

impl Stream for AnthropicSSEStream {
    type Item = Delta;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(delta) = self.pending.pop_front() {
                return Poll::Ready(Some(delta));
            }
            if self.done {
                return Poll::Ready(None);
            }

            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(Ok(chunk)))) => {
                    self.buf.extend_from_slice(&chunk);
                    while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                        let mut raw: Vec<u8> = self.buf.drain(..=pos).collect();
                        while matches!(raw.last(), Some(&b'\r') | Some(&b'\n')) {
                            raw.pop();
                        }
                        // Decode only at complete line boundaries so a
                        // multi-byte UTF-8 char split across TCP chunks stays
                        // intact.
                        let line = String::from_utf8(raw).unwrap_or_else(|e| {
                            String::from_utf8_lossy(e.as_bytes()).into_owned()
                        });
                        if self.process_line(&line) {
                            self.done = true;
                        }
                    }
                }
                Poll::Ready(Some(Ok(Err(_)))) => {
                    self.pending.push_back(Delta {
                        role: "assistant".into(),
                        content: None,
                        reasoning: None,
                        tool_calls: None,
                        finish_reason: Some("error".into()),
                        usage: None,
                        error_type: Some("request".into()),
                    });
                    self.done = true;
                }
                // The idle-read timeout fired: see openai_compat.rs.
                Poll::Ready(Some(Err(_elapsed))) => {
                    self.pending.push_back(Delta {
                        role: "assistant".into(),
                        content: None,
                        reasoning: None,
                        tool_calls: None,
                        finish_reason: Some("error".into()),
                        usage: None,
                        error_type: Some("request".into()),
                    });
                    self.done = true;
                }
                Poll::Ready(None) => {
                    // A trailing complete line with no final newline is still
                    // a real SSE line — decode it before finishing.
                    if !self.buf.is_empty() {
                        let raw = std::mem::take(&mut self.buf);
                        let line = String::from_utf8(raw).unwrap_or_else(|e| {
                            String::from_utf8_lossy(e.as_bytes()).into_owned()
                        });
                        if self.process_line(&line) {
                            self.done = true;
                        }
                    }
                    // Drain any Delta the trailing line produced before
                    // reporting end-of-stream.
                    if !self.pending.is_empty() {
                        self.done = true;
                        continue;
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }
    }
}

#[cfg(test)]
mod sse_tests {
    use super::*;
    use futures::{stream, StreamExt};

    #[tokio::test]
    async fn multiple_data_lines_in_one_chunk_are_all_emitted() {
        let chunk = "event: content_block_delta\n\
                     data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n\
                     event: content_block_delta\n\
                     data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" there\"}}\n\n";
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_anthropic_sse(inner, Duration::from_secs(300)).collect().await;
        let contents: Vec<String> = deltas.into_iter().filter_map(|d| d.content).collect();
        assert_eq!(contents, vec!["Hi".to_string(), " there".to_string()]);
    }

    #[tokio::test]
    async fn data_line_split_across_chunk_boundary_still_parses() {
        let chunk1 = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"te";
        let chunk2 = "xt\":\"Hello\"}}\n\n";
        let inner = stream::iter(vec![
            Ok::<bytes::Bytes, reqwest::Error>(bytes::Bytes::from(chunk1)),
            Ok::<bytes::Bytes, reqwest::Error>(bytes::Bytes::from(chunk2)),
        ]);
        let deltas: Vec<Delta> = parse_anthropic_sse(inner, Duration::from_secs(300)).collect().await;
        let contents: Vec<String> = deltas.into_iter().filter_map(|d| d.content).collect();
        assert_eq!(contents, vec!["Hello".to_string()]);
    }

    #[tokio::test]
    async fn multi_byte_utf8_split_across_chunks_round_trips() {
        // The é (U+00E9 = UTF-8 0xC3 0xA9) is split across the two chunks —
        // per-chunk `String::from_utf8_lossy` used to corrupt it.
        let mut chunk1 =
            b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"caf"
                .to_vec();
        chunk1.push(0xC3);
        let chunk2 = b"\xA9\"}}\n\n".to_vec();
        let inner = stream::iter(vec![
            Ok::<bytes::Bytes, reqwest::Error>(bytes::Bytes::from(chunk1)),
            Ok::<bytes::Bytes, reqwest::Error>(bytes::Bytes::from(chunk2)),
        ]);
        let deltas: Vec<Delta> = parse_anthropic_sse(inner, Duration::from_secs(300))
            .collect()
            .await;
        let contents: String = deltas.iter().filter_map(|d| d.content.clone()).collect();
        assert_eq!(contents, "café");
    }

    #[tokio::test]
    async fn idle_read_timeout_terminates_a_silent_stream_as_a_transient_error() {
        let silent = stream::pending::<Result<bytes::Bytes, reqwest::Error>>();
        let deltas: Vec<Delta> = parse_anthropic_sse(silent, Duration::from_millis(50))
            .collect()
            .await;
        let last = deltas.last().expect("expected at least the error Delta");
        assert_eq!(last.error_type.as_deref(), Some("request"));
        assert_eq!(last.finish_reason.as_deref(), Some("error"));
    }

    #[tokio::test]
    async fn message_stop_ends_stream() {
        let chunk = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n\
                     data: {\"type\":\"message_stop\"}\n\n";
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_anthropic_sse(inner, Duration::from_secs(300)).collect().await;
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].content.as_deref(), Some("Hi"));
    }

    #[tokio::test]
    async fn trailing_complete_line_without_newline_is_still_decoded() {
        // A final `data:` line with no trailing `\n` is still a real SSE
        // line — it must be decoded and surfaced at end-of-stream.
        let chunk = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}";
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_anthropic_sse(inner, Duration::from_secs(300))
            .collect()
            .await;
        let contents: Vec<String> = deltas.into_iter().filter_map(|d| d.content).collect();
        assert_eq!(contents, vec!["Hi".to_string()]);
    }

    #[tokio::test]
    async fn tool_call_captures_name_and_wraps_arguments() {
        let chunk = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\"}}\n\n\
                     data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n\
                     data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"a.txt\\\"}\"}}\n\n\
                     data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                     data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n";
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_anthropic_sse(inner, Duration::from_secs(300)).collect().await;
        let tool_calls = deltas
            .into_iter()
            .find_map(|d| d.tool_calls)
            .expect("expected a Delta carrying tool_calls");
        assert_eq!(tool_calls.len(), 1);
        let tc = &tool_calls[0];
        assert_eq!(tc.id, "toolu_1");
        assert_eq!(
            tc.function.get("name").and_then(|v| v.as_str()),
            Some("read_file")
        );
        assert_eq!(
            tc.function
                .get("arguments")
                .and_then(|v| v.get("path"))
                .and_then(|v| v.as_str()),
            Some("a.txt")
        );
    }

    #[tokio::test]
    async fn message_start_captures_input_and_cache_tokens() {
        let chunk = "data: {\"type\":\"message_start\",\"message\":{\"role\":\"assistant\",\"usage\":{\"input_tokens\":12,\"output_tokens\":1,\"cache_creation_input_tokens\":340,\"cache_read_input_tokens\":890}}}\n\n";
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_anthropic_sse(inner, Duration::from_secs(300)).collect().await;
        let usage = deltas[0]
            .usage
            .as_ref()
            .expect("expected usage on message_start Delta");
        // Normalized to total prompt size (fresh + cache_creation +
        // cache_read = 12 + 340 + 890), matching OpenAI's `prompt_tokens`
        // semantics — see `usage_map_from_anthropic`'s doc comment.
        assert_eq!(usage.get("input_tokens"), Some(&1242));
        assert_eq!(usage.get("cache_creation_tokens"), Some(&340));
        assert_eq!(usage.get("cache_read_tokens"), Some(&890));
    }

    #[tokio::test]
    async fn message_delta_captures_final_output_tokens() {
        let chunk = "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":57}}\n\n";
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_anthropic_sse(inner, Duration::from_secs(300)).collect().await;
        let usage = deltas[0]
            .usage
            .as_ref()
            .expect("expected usage on message_delta Delta");
        assert_eq!(usage.get("output_tokens"), Some(&57));
    }

    #[tokio::test]
    async fn all_system_messages_are_joined_not_just_the_first() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/messages")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"system": "persona\n\ntool hints\n\nmemory"}),
            ))
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body("data: {\"type\":\"message_stop\"}\n\n")
            .create_async()
            .await;

        let provider = AnthropicProvider::new(
            "test",
            ProviderConfig {
                base_url: server.url(),
                ..Default::default()
            },
            Arc::new(TailscaleClient::new()),
            CacheConfig::default(),
        );
        let messages = vec![
            serde_json::json!({"role": "system", "content": "persona"}),
            serde_json::json!({"role": "system", "content": "tool hints"}),
            serde_json::json!({"role": "system", "content": "memory"}),
            serde_json::json!({"role": "user", "content": "hi"}),
        ];
        let stream = provider
            .chat_completion(messages, None, SamplingParams::default(), None, None)
            .await
            .unwrap();
        let _: Vec<Delta> = stream.collect().await;

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn system_gets_a_cache_control_breakpoint_when_enabled() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/messages")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "system": [{
                    "type": "text",
                    "text": "persona",
                    "cache_control": {"type": "ephemeral"},
                }],
            })))
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body("data: {\"type\":\"message_stop\"}\n\n")
            .create_async()
            .await;

        let provider = AnthropicProvider::new(
            "test",
            ProviderConfig {
                base_url: server.url(),
                ..Default::default()
            },
            Arc::new(TailscaleClient::new()),
            CacheConfig {
                anthropic_cache_control: true,
                ..Default::default()
            },
        );
        let messages = vec![
            serde_json::json!({"role": "system", "content": "persona"}),
            serde_json::json!({"role": "user", "content": "hi"}),
        ];
        let stream = provider
            .chat_completion(messages, None, SamplingParams::default(), None, None)
            .await
            .unwrap();
        let _: Vec<Delta> = stream.collect().await;

        mock.assert_async().await;
    }
}
