use async_trait::async_trait;
use futures::Stream;
use serde_json::Value;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use super::base::{
    classify_provider_error, Delta, HealthStatus, ModelInfo, Provider, SamplingParams,
};
use crate::config::ProviderConfig;
use crate::error::ProviderError;
use crate::network::{maybe_direct_url, TailscaleClient};

pub struct OpenAICompatibleProvider {
    pub provider_id: String,
    pub config: ProviderConfig,
    client: reqwest::Client,
    tailscale: Arc<TailscaleClient>,
}

impl OpenAICompatibleProvider {
    pub const DEFAULT_MODEL: &'static str = "gpt-4o";

    pub fn new(provider_id: &str, config: ProviderConfig, tailscale: Arc<TailscaleClient>) -> Self {
        Self {
            provider_id: provider_id.into(),
            client: reqwest::Client::new(),
            config,
            tailscale,
        }
    }

    /// Length of the longest suffix of `s` that is also a proper prefix of
    /// `tag` — used to detect "the tail of this fragment might be the start
    /// of a tag that continues in the next SSE chunk".
    fn longest_tag_prefix_suffix(s: &str, tag: &str) -> usize {
        let max = tag.len().saturating_sub(1).min(s.len());
        for len in (1..=max).rev() {
            if s.ends_with(&tag[..len]) {
                return len;
            }
        }
        0
    }

    /// Merges every `role: "system"` message into a single one at the front
    /// of the array, preserving the relative order of everything else.
    /// Context building layers up to 5 separate system messages (persona,
    /// session override, writable-dir hint, anchored first message,
    /// consolidated memory — see `agent/context/builder.rs`, plus an
    /// occasional mid-conversation one from `emergency_trim`). Many chat
    /// templates only special-case `messages[0]` (and sometimes `[1]`) as a
    /// mergeable leading system turn and silently drop any system-role
    /// message beyond that position instead of rendering it — observed on
    /// Qwen's official ChatML/tool-calling template, whose `num_sys` never
    /// exceeds 2. Left alone, everything past the second system layer just
    /// vanishes from the model's context on affected backends, and the
    /// dangling `<|im_start|>system` markup around what *does* survive can
    /// leave the model confused about turn boundaries. Mirrors what
    /// `AnthropicProvider::chat_completion` already does when collapsing
    /// system messages into Anthropic's single `system` string field.
    fn merge_system_messages(messages: Vec<Value>) -> Vec<Value> {
        let mut system_parts: Vec<String> = Vec::new();
        let mut rest: Vec<Value> = Vec::new();
        for msg in messages {
            if msg["role"] == "system" {
                if let Some(c) = msg["content"].as_str() {
                    system_parts.push(c.to_string());
                }
            } else {
                rest.push(msg);
            }
        }
        if system_parts.is_empty() {
            return rest;
        }
        let mut merged = vec![serde_json::json!({
            "role": "system",
            "content": system_parts.join("\n\n"),
        })];
        merged.extend(rest);
        merged
    }

    /// If `url`'s host is a Tailscale peer with a discoverable direct (LAN)
    /// address, tries that address first (bounded by
    /// `network::DIRECT_CONNECT_TIMEOUT`) and falls back to the original
    /// (tunneled) URL on any error. A no-op — single request, original URL —
    /// for every other host (localhost, non-Tailscale, or no direct address
    /// known). Mirrors Python's `PreferDirectTransport`.
    async fn send_preferring_direct(
        &self,
        url: &str,
        body: &Value,
    ) -> Result<reqwest::Response, reqwest::Error> {
        if let Some(direct_url) = maybe_direct_url(&self.tailscale, url).await {
            let direct = self
                .client
                .post(&direct_url)
                .header("Authorization", format!("Bearer {}", self.config.api_key))
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
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .json(body)
            .send()
            .await
    }
}

#[async_trait]
impl Provider for OpenAICompatibleProvider {
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

    async fn chat_completion(
        &self,
        messages: Vec<Value>,
        tools: Option<Vec<Value>>,
        sampling: SamplingParams,
        model: Option<String>,
        id_slot: Option<i32>,
    ) -> Result<Pin<Box<dyn Stream<Item = Delta> + Send>>, ProviderError> {
        let model = self.resolve_model(model.as_deref());
        let url = format!("{}/v1/chat/completions", self.config.base_url);
        let messages = Self::merge_system_messages(messages);

        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
        });

        if let Some(t) = tools {
            body["tools"] = t.into();
        }
        if let Some(t) = sampling.temperature {
            body["temperature"] = t.into();
        }
        if let Some(t) = sampling.top_p {
            body["top_p"] = t.into();
        }
        if let Some(p) = sampling.presence_penalty {
            body["presence_penalty"] = p.into();
        }
        if let Some(f) = sampling.frequency_penalty {
            body["frequency_penalty"] = f.into();
        }
        if let Some(m) = sampling.max_tokens {
            body["max_tokens"] = m.into();
        }
        // top_k/min_p are llama.cpp/Ollama extensions to the OpenAI-compatible
        // wire format, not part of the spec — a hosted OpenAI-compatible
        // endpoint could reject an unrecognized field. `sampling::defaults_for`
        // only ever fills these for `provider_type` "ollama"/"custom_openai",
        // but a user-typed `temperature`/`top_k` override on a mislabeled
        // profile is still possible, so gate on the wire dialect here too.
        if matches!(
            self.config.provider_type.as_str(),
            "ollama" | "custom_openai"
        ) {
            if let Some(k) = sampling.top_k {
                body["top_k"] = k.into();
            }
            if let Some(p) = sampling.min_p {
                body["min_p"] = p.into();
            }
        }
        if let Some(slot) = id_slot {
            body["id_slot"] = slot.into();
        }

        tracing::debug!(
            provider_id = %self.provider_id,
            body = %body,
            "chat_completion request body"
        );

        let resp = self
            .send_preferring_direct(&url, &body)
            .await
            .map_err(|e| ProviderError::Request {
                user_message: format!("Failed to connect to provider: {}", e),
                raw_message: e.to_string(),
                http_status: 0,
            })?;

        let status_code = resp.status().as_u16();
        if !resp.status().is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(classify_provider_error(status_code, &body_text));
        }

        // Use bytes_stream from the stream feature
        let stream = resp.bytes_stream();
        let deltas = parse_openai_sse(stream);
        Ok(Box::pin(deltas))
    }

    async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let url = format!("{}/v1/models", self.config.base_url);
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
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
                            context_length: m["max_model_len"]
                                .as_i64()
                                .map(|v| v as i32)
                                .or_else(|| m["context_length"].as_i64().map(|v| v as i32)),
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
            .header("Authorization", format!("Bearer {}", self.config.api_key))
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

/// Parse OpenAI-compatible SSE stream into Delta chunks.
///
/// Buffers a trailing partial line across polls (`buf`) and queues every Delta
/// produced while parsing one chunk (`pending`) since a single chunk routinely
/// contains multiple `data:` frames — returning after the first one silently
/// dropped the rest. `Poll::Pending` from the inner stream is propagated as-is,
/// never conflated with end-of-stream.
/// Accumulates one streamed tool call across however many `tool_calls[]`
/// delta fragments it arrives in. The server sends `name` once, in the
/// first fragment for a given `index` — everything after that is
/// incremental `arguments` text only.
#[derive(Default)]
struct PendingToolCall {
    id: Option<String>,
    r#type: Option<String>,
    name: Option<String>,
    arguments: String,
}

struct OpenAISSEStream {
    inner: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>,
    tool_call_buf: HashMap<usize, PendingToolCall>,
    buf: String,
    pending: std::collections::VecDeque<Delta>,
    done: bool,
    /// The most recent non-null `finish_reason` seen on any choice. A
    /// `finish_reason` chunk (e.g. `"tool_calls"`) routinely arrives in the
    /// same or a later SSE line than the final tool-call-argument fragment,
    /// while `tool_call_buf` is still non-empty — previously this meant it
    /// only ever got attached to a Delta when the buffer *happened* to
    /// already be empty (essentially never, for a tool-calling turn), and
    /// was otherwise silently dropped instead of surfacing on the `[DONE]`
    /// tool-calls flush.
    last_finish_reason: Option<String>,
    /// Whether the last fragment left us mid-`<think>...</think>` block —
    /// carried across `process_line` calls (i.e. across SSE chunks), since a
    /// single reasoning block routinely spans many deltas. Resetting this
    /// per-fragment (the old behavior) meant every fragment after the first
    /// one containing `<think>` was treated as plain output.
    in_thinking: bool,
    /// Trailing text held back from the previous fragment because it could
    /// be the start of a `<think>`/`</think>` tag split across a chunk
    /// boundary — prepended to the next fragment before re-scanning.
    pending_tag_prefix: String,
}

fn parse_openai_sse(
    stream: impl Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
) -> OpenAISSEStream {
    OpenAISSEStream {
        inner: Box::pin(stream),
        tool_call_buf: HashMap::new(),
        buf: String::new(),
        pending: std::collections::VecDeque::new(),
        done: false,
        last_finish_reason: None,
        in_thinking: false,
        pending_tag_prefix: String::new(),
    }
}

impl OpenAISSEStream {
    /// Split `<think>...</think>` thinking tags out of one content fragment.
    /// A single `str::find` scan per tag occurrence (not a re-clone-and-scan
    /// per character), and `in_thinking`/`pending_tag_prefix` on `self` carry
    /// state across fragments so a tag spanning multiple SSE deltas — the
    /// normal case — is still recognized as one tag.
    fn split_thinking_tags(&mut self, content: &str) -> (String, Option<String>) {
        const OPEN_TAG: &str = "<think>";
        const CLOSE_TAG: &str = "</think>";

        let combined = if self.pending_tag_prefix.is_empty() {
            content.to_string()
        } else {
            let mut s = std::mem::take(&mut self.pending_tag_prefix);
            s.push_str(content);
            s
        };

        let mut text = String::new();
        let mut thinking = String::new();
        let mut rest: &str = &combined;

        loop {
            let needle = if self.in_thinking {
                CLOSE_TAG
            } else {
                OPEN_TAG
            };
            match rest.find(needle) {
                Some(idx) => {
                    let (before, after) = rest.split_at(idx);
                    if self.in_thinking {
                        thinking.push_str(before);
                    } else {
                        text.push_str(before);
                    }
                    self.in_thinking = !self.in_thinking;
                    rest = &after[needle.len()..];
                }
                None => {
                    let hold = OpenAICompatibleProvider::longest_tag_prefix_suffix(rest, needle);
                    let (keep, hold_str) = rest.split_at(rest.len() - hold);
                    if self.in_thinking {
                        thinking.push_str(keep);
                    } else {
                        text.push_str(keep);
                    }
                    self.pending_tag_prefix = hold_str.to_string();
                    break;
                }
            }
        }

        let reasoning = if thinking.is_empty() {
            None
        } else {
            Some(thinking)
        };
        (text, reasoning)
    }
    /// Process one complete SSE line, pushing any resulting Delta(s) onto `pending`.
    /// Returns true if this line signalled stream completion (`[DONE]`).
    fn process_line(&mut self, line: &str) -> bool {
        let Some(data) = line.strip_prefix("data: ") else {
            return false;
        };

        if data == "[DONE]" {
            if !self.tool_call_buf.is_empty() {
                let tool_calls: Vec<super::base::ToolCall> = self
                    .tool_call_buf
                    .drain()
                    .map(|(_, buf)| {
                        // `function` must carry BOTH `name` and `arguments` — the
                        // agent loop reads `tc.function.get("name")` /
                        // `.get("arguments")`. The previous version set
                        // `function` to just the parsed arguments object with no
                        // `name` key at all (and never captured the streamed
                        // `function.name` fragment in the first place), so every
                        // tool call executed as an unnamed/"unknown" tool.
                        let arguments: Value = serde_json::from_str(&buf.arguments)
                            .unwrap_or_else(|_| serde_json::json!({}));
                        let function = serde_json::json!({
                            "name": buf.name.unwrap_or_default(),
                            "arguments": arguments,
                        });
                        super::base::ToolCall {
                            id: buf.id.unwrap_or_default(),
                            r#type: buf.r#type.unwrap_or_else(|| "function".into()),
                            function,
                        }
                    })
                    .collect();
                self.pending.push_back(Delta {
                    role: "assistant".into(),
                    content: None,
                    reasoning: None,
                    tool_calls: Some(tool_calls),
                    finish_reason: self.last_finish_reason.take(),
                    usage: None,
                    error_type: None,
                });
            }
            return true;
        }

        let json: Value = match serde_json::from_str(data) {
            Ok(j) => j,
            Err(_) => return false,
        };

        // Standard OpenAI-compatible endpoints send `usage` and the final
        // `choices[].delta.finish_reason` in the SAME chunk. Returning early
        // here used to skip the `choices` parsing below entirely, so the
        // finish reason was never observed, the tool loop never saw a
        // reason to stop, and it kept re-calling the LLM until `max_steps`.
        // Queue the usage Delta but fall through to also process `choices`.
        if let Some(usage) = json.get("usage") {
            let mut usage_map = HashMap::new();
            if let Some(i) = usage["prompt_tokens"].as_i64() {
                usage_map.insert("input_tokens".into(), i as i32);
            }
            if let Some(i) = usage["completion_tokens"].as_i64() {
                usage_map.insert("output_tokens".into(), i as i32);
            }
            // OpenAI-style prompt-cache reporting (also mirrored by some
            // llama.cpp/vLLM-compatible endpoints): tokens served from cache
            // are a *subset* of `prompt_tokens` above, not additional to it —
            // unlike Anthropic, where `input_tokens` already excludes them.
            if let Some(i) = usage["prompt_tokens_details"]["cached_tokens"].as_i64() {
                usage_map.insert("cache_read_tokens".into(), i as i32);
            }
            if !usage_map.is_empty() {
                self.pending.push_back(Delta {
                    role: "assistant".into(),
                    content: None,
                    reasoning: None,
                    tool_calls: None,
                    finish_reason: None,
                    usage: Some(usage_map),
                    error_type: None,
                });
            }
        }

        if let Some(choices) = json["choices"].as_array() {
            for choice in choices {
                let delta = &choice["delta"];
                let role = delta["role"].as_str().unwrap_or("assistant").into();
                let mut content = delta["content"].as_str().map(|s| s.to_string());
                let mut reasoning = delta["reasoning_content"]
                    .as_str()
                    .or(delta["reasoning"].as_str())
                    .map(|s| s.to_string());

                if let Some(c) = content.take() {
                    let (text, think) = self.split_thinking_tags(&c);
                    content = if text.is_empty() { None } else { Some(text) };
                    if let Some(t) = think {
                        reasoning = Some(reasoning.map(|r| r + &t).unwrap_or(t));
                    }
                }

                if let Some(tc) = delta["tool_calls"].as_array() {
                    for t in tc {
                        let index = t["index"].as_u64().unwrap_or(0) as usize;
                        let entry = self.tool_call_buf.entry(index).or_default();
                        if let Some(i) = t["id"].as_str() {
                            entry.id = Some(i.into());
                        }
                        if let Some(tp) = t["type"].as_str() {
                            entry.r#type = Some(tp.into());
                        }
                        // The tool name arrives once, in the first fragment
                        // for this index — it was never captured before,
                        // which is exactly why every tool call executed
                        // nameless/"unknown".
                        if let Some(name) = t["function"]["name"].as_str() {
                            entry.name = Some(name.into());
                        }
                        if let Some(f) = t["function"]["arguments"].as_str() {
                            entry.arguments.push_str(f);
                        }
                    }
                }

                let finish_reason: Option<String> =
                    choice["finish_reason"].as_str().map(|s| s.into());
                if let Some(ref fr) = finish_reason {
                    self.last_finish_reason = Some(fr.clone());
                }

                // Nothing meaningful to emit for this choice on its own —
                // a tool-call-argument fragment (if any) has already been
                // folded into `tool_call_buf` above, and `finish_reason` (if
                // present) has just been remembered for whichever Delta
                // ends up completing the turn (either right here, or the
                // `[DONE]` tool-calls flush above).
                if content.is_none() && reasoning.is_none() && finish_reason.is_none() {
                    continue;
                }

                self.pending.push_back(Delta {
                    role,
                    content,
                    reasoning,
                    tool_calls: None,
                    finish_reason,
                    usage: None,
                    error_type: None,
                });
            }
        }

        false
    }
}

impl Stream for OpenAISSEStream {
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
                Poll::Ready(Some(Ok(chunk))) => {
                    self.buf.push_str(&String::from_utf8_lossy(&chunk));
                    while let Some(pos) = self.buf.find('\n') {
                        let raw: String = self.buf.drain(..=pos).collect();
                        let trimmed = raw.trim_end_matches(['\r', '\n']);
                        if self.process_line(trimmed) {
                            self.done = true;
                        }
                    }
                    // Loop back: drain anything just queued, or poll inner again if
                    // this chunk had no complete `data:` line yet.
                }
                Poll::Ready(Some(Err(_))) => {
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
        let chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n\
                     data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n";
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_openai_sse(inner).collect().await;
        let contents: Vec<String> = deltas.into_iter().filter_map(|d| d.content).collect();
        assert_eq!(contents, vec!["Hello".to_string(), " world".to_string()]);
    }

    #[tokio::test]
    async fn data_line_split_across_chunk_boundary_still_parses() {
        let chunk1 = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel";
        let chunk2 = "lo\"}}]}\n\n";
        let inner = stream::iter(vec![
            Ok::<bytes::Bytes, reqwest::Error>(bytes::Bytes::from(chunk1)),
            Ok::<bytes::Bytes, reqwest::Error>(bytes::Bytes::from(chunk2)),
        ]);
        let deltas: Vec<Delta> = parse_openai_sse(inner).collect().await;
        let contents: Vec<String> = deltas.into_iter().filter_map(|d| d.content).collect();
        assert_eq!(contents, vec!["Hello".to_string()]);
    }

    #[tokio::test]
    async fn done_marker_ends_stream() {
        let chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\ndata: [DONE]\n\n";
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_openai_sse(inner).collect().await;
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].content.as_deref(), Some("Hi"));
    }

    #[tokio::test]
    async fn tool_call_captures_name_and_wraps_arguments() {
        // Realistic OpenAI streaming shape: name arrives once in the first
        // fragment, arguments accumulate across several fragments.
        let chunk = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]}}]}\n\n\
                     data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n\
                     data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a.txt\\\"}\"}}]}}]}\n\n\
                     data: [DONE]\n\n";
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_openai_sse(inner).collect().await;
        let tool_calls = deltas
            .into_iter()
            .find_map(|d| d.tool_calls)
            .expect("expected a Delta carrying tool_calls");
        assert_eq!(tool_calls.len(), 1);
        let tc = &tool_calls[0];
        assert_eq!(tc.id, "call_1");
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
    async fn think_tag_split_across_sse_chunks_is_still_recognized() {
        // Realistic streaming shape: the model emits `<think>` in one
        // fragment, reasoning content and the closing `</think>` arrive in
        // later fragments. Each `data:` line here is its own SSE event/Delta,
        // exercising `in_thinking` state that must survive across them.
        let chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"<think>\"}}]}\n\n\
                     data: {\"choices\":[{\"delta\":{\"content\":\"reasoning here\"}}]}\n\n\
                     data: {\"choices\":[{\"delta\":{\"content\":\"</think>answer\"}}]}\n\n\
                     data: [DONE]\n\n";
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_openai_sse(inner).collect().await;
        let content: String = deltas.iter().filter_map(|d| d.content.clone()).collect();
        let reasoning: String = deltas.iter().filter_map(|d| d.reasoning.clone()).collect();
        assert_eq!(content, "answer");
        assert_eq!(reasoning, "reasoning here");
    }

    #[tokio::test]
    async fn think_tag_split_mid_tag_across_sse_chunks_is_still_recognized() {
        // The tag delimiter itself splits across the chunk boundary, not
        // just the reasoning content between tags.
        let chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"<thi\"}}]}\n\n\
                     data: {\"choices\":[{\"delta\":{\"content\":\"nk>secret</th\"}}]}\n\n\
                     data: {\"choices\":[{\"delta\":{\"content\":\"ink>answer\"}}]}\n\n\
                     data: [DONE]\n\n";
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_openai_sse(inner).collect().await;
        let content: String = deltas.iter().filter_map(|d| d.content.clone()).collect();
        let reasoning: String = deltas.iter().filter_map(|d| d.reasoning.clone()).collect();
        assert_eq!(content, "answer");
        assert_eq!(reasoning, "secret");
    }

    #[tokio::test]
    async fn cached_tokens_are_captured_from_prompt_tokens_details() {
        let chunk = "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":20,\"prompt_tokens_details\":{\"cached_tokens\":64}}}\n\ndata: [DONE]\n\n";
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_openai_sse(inner).collect().await;
        let usage = deltas
            .iter()
            .find_map(|d| d.usage.as_ref())
            .expect("expected a Delta carrying usage");
        assert_eq!(usage.get("input_tokens"), Some(&100));
        assert_eq!(usage.get("cache_read_tokens"), Some(&64));
    }

    #[tokio::test]
    async fn finish_reason_survives_a_tool_calling_turn() {
        // The terminal chunk carries `finish_reason: "tool_calls"` alongside
        // an empty delta (the realistic OpenAI shape) — previously this was
        // dropped entirely whenever `tool_call_buf` was non-empty, which is
        // the case on essentially every real tool-calling turn.
        let chunk = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}]}}]}\n\n\
                     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                     data: [DONE]\n\n";
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_openai_sse(inner).collect().await;
        assert!(
            deltas
                .iter()
                .any(|d| d.finish_reason.as_deref() == Some("tool_calls")),
            "expected some Delta to carry finish_reason \"tool_calls\", got {deltas:?}"
        );
    }

    #[tokio::test]
    async fn id_slot_is_included_in_the_request_body_when_provided() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"id_slot": 2}),
            ))
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body("data: [DONE]\n\n")
            .create_async()
            .await;

        let provider = OpenAICompatibleProvider::new(
            "test",
            crate::config::ProviderConfig {
                base_url: server.url(),
                ..Default::default()
            },
            Arc::new(TailscaleClient::new()),
        );
        let messages = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let stream = provider
            .chat_completion(messages, None, SamplingParams::default(), None, Some(2))
            .await
            .unwrap();
        let _: Vec<Delta> = stream.collect().await;

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn id_slot_is_absent_from_the_request_body_by_default() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .match_body(mockito::Matcher::Json(serde_json::json!({
                "model": OpenAICompatibleProvider::DEFAULT_MODEL,
                "messages": [{"role": "user", "content": "hi"}],
                "stream": true,
                "stream_options": {"include_usage": true},
            })))
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body("data: [DONE]\n\n")
            .create_async()
            .await;

        let provider = OpenAICompatibleProvider::new(
            "test",
            crate::config::ProviderConfig {
                base_url: server.url(),
                ..Default::default()
            },
            Arc::new(TailscaleClient::new()),
        );
        let messages = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let stream = provider
            .chat_completion(messages, None, SamplingParams::default(), None, None)
            .await
            .unwrap();
        let _: Vec<Delta> = stream.collect().await;

        mock.assert_async().await;
    }

    /// Covers `send_preferring_direct`'s "prefer the direct LAN shortcut"
    /// path end-to-end: a `base_url` whose host is a Tailscale address, with
    /// a direct address seeded (see `TailscaleClient::seed_resolved_for_test`
    /// in `network.rs`, which bypasses the real local-Tailscale-API/DNS
    /// lookups the production path uses to find one) — the request must
    /// land on the direct address, never the original. The `base_url` host
    /// (`100.64.1.2`) is never actually dialed in this test: `send_preferring_direct`
    /// returns as soon as the direct attempt succeeds, so it doesn't need to
    /// be a real, reachable address.
    ///
    /// The mirror case — direct fails, falls back to the original Tailscale
    /// address — isn't covered by an HTTP-level test: that fallback request
    /// targets the literal configured `base_url` host, which must itself be
    /// a real Tailscale peer address to exercise `maybe_direct_url` at all,
    /// and there's no way to make that reachable from a unit test without a
    /// real Tailscale network. The fallback *control flow* itself (`if let
    /// Ok(resp) = direct { return Ok(resp) }`, falling through otherwise) is
    /// a two-line, directly-readable guard; `network.rs`'s
    /// `maybe_direct_url_is_none_when_no_direct_address_is_known` covers the
    /// upstream case that makes this function skip the direct attempt
    /// entirely and go straight to the original.
    #[tokio::test]
    async fn send_preferring_direct_prefers_a_reachable_direct_address_over_the_original() {
        let mut direct_server = mockito::Server::new_with_opts_async(mockito::ServerOpts {
            host: "127.0.0.2",
            ..Default::default()
        })
        .await;
        let direct_mock = direct_server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body("data: [DONE]\n\n")
            .expect(1)
            .create_async()
            .await;

        let direct_port = direct_server
            .host_with_port()
            .rsplit(':')
            .next()
            .unwrap()
            .to_string();

        let tailscale = Arc::new(TailscaleClient::new());
        tailscale.seed_resolved_for_test("100.64.1.2", Some("127.0.0.2".to_string()));

        let provider = OpenAICompatibleProvider::new(
            "test",
            crate::config::ProviderConfig {
                // Never actually dialed — see the doc comment above.
                base_url: format!("http://100.64.1.2:{direct_port}"),
                ..Default::default()
            },
            tailscale,
        );
        let messages = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let stream = provider
            .chat_completion(messages, None, SamplingParams::default(), None, None)
            .await
            .unwrap();
        let _: Vec<Delta> = stream.collect().await;

        direct_mock.assert_async().await;
    }

    /// A plain, non-Tailscale `base_url` (the common case — everyone not
    /// deliberately pointed at a Tailscale peer) must behave exactly as if
    /// `send_preferring_direct` didn't exist: `maybe_direct_url` returns
    /// `None` immediately (host isn't in the Tailscale CGNAT range) and the
    /// request goes straight to the configured URL, unchanged. Every other
    /// test in this module already exercises this path implicitly (their
    /// `base_url` is always a plain `mockito` `server.url()`, i.e.
    /// `127.0.0.1`); this test names the property explicitly.
    #[tokio::test]
    async fn send_preferring_direct_is_a_pure_pass_through_for_a_non_tailscale_host() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body("data: [DONE]\n\n")
            .expect(1)
            .create_async()
            .await;

        let provider = OpenAICompatibleProvider::new(
            "test",
            crate::config::ProviderConfig {
                base_url: server.url(),
                ..Default::default()
            },
            Arc::new(TailscaleClient::new()),
        );
        let messages = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let stream = provider
            .chat_completion(messages, None, SamplingParams::default(), None, None)
            .await
            .unwrap();
        let _: Vec<Delta> = stream.collect().await;

        mock.assert_async().await;
    }

    #[test]
    fn merge_system_messages_joins_every_system_entry_into_one_leading_message() {
        let messages = vec![
            serde_json::json!({"role": "system", "content": "persona"}),
            serde_json::json!({"role": "system", "content": "tool hints"}),
            serde_json::json!({"role": "user", "content": "hi"}),
            serde_json::json!({"role": "system", "content": "memory"}),
            serde_json::json!({"role": "assistant", "content": "hello"}),
        ];
        let merged = OpenAICompatibleProvider::merge_system_messages(messages);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0]["role"], "system");
        assert_eq!(merged[0]["content"], "persona\n\ntool hints\n\nmemory");
        assert_eq!(merged[1]["role"], "user");
        assert_eq!(merged[2]["role"], "assistant");
    }

    #[test]
    fn merge_system_messages_leaves_a_system_free_array_untouched() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "hi"}),
            serde_json::json!({"role": "assistant", "content": "hello"}),
        ];
        let merged = OpenAICompatibleProvider::merge_system_messages(messages.clone());
        assert_eq!(merged, messages);
    }

    #[tokio::test]
    async fn all_system_messages_are_joined_not_just_the_first() {
        // Backs the fix for chat templates (observed: Qwen's official
        // ChatML/tool-calling template) that only special-case messages[0]
        // (and sometimes [1]) as a mergeable leading system turn and
        // silently drop any system-role message beyond that position —
        // context building layers up to 5 separate system messages, so
        // keeping them as separate array entries meant everything past the
        // first one or two vanished from the model's context on affected
        // backends.
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/v1/chat/completions")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "messages": [
                    {"role": "system", "content": "persona\n\ntool hints\n\nmemory"},
                    {"role": "user", "content": "hi"},
                ],
            })))
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body("data: [DONE]\n\n")
            .create_async()
            .await;

        let provider = OpenAICompatibleProvider::new(
            "test",
            crate::config::ProviderConfig {
                base_url: server.url(),
                ..Default::default()
            },
            Arc::new(TailscaleClient::new()),
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
}
