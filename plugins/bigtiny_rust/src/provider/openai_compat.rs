use async_trait::async_trait;
use futures::Stream;
use serde_json::Value;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use super::base::{
    classify_provider_error, classify_transport_error, parse_retry_after, read_bounded_error_body,
    Delta, Effort, HealthStatus, ModelInfo, Provider, SamplingParams,
};
use crate::config::ProviderConfig;
use super::tag_split::TagSplitter;
use crate::error::ProviderError;
use crate::network::{maybe_direct_url, TailscaleClient};

/// OpenRouter's nested `reasoning` object for an effort level. OpenRouter is
/// the one dialect here that can switch reasoning off explicitly. A model-
/// specific `Custom` level is clamped to `high` (see `Effort::hosted_level`).
fn openrouter_reasoning(e: &Effort) -> Value {
    match e.hosted_level() {
        None => serde_json::json!({ "enabled": false }),
        Some(level) => serde_json::json!({ "effort": level }),
    }
}

pub struct OpenAICompatibleProvider {
    pub provider_id: String,
    pub config: ProviderConfig,
    client: reqwest::Client,
    /// Dedicated client for the Tailscale direct-address attempt. It bounds
    /// ONLY the connect phase (`network::DIRECT_CONNECT_TIMEOUT`) so an
    /// unreachable LAN address falls back to the tunnel quickly — crucially
    /// WITHOUT a reqwest request-level timeout, which also covers the
    /// response body: the old `.timeout(DIRECT_CONNECT_TIMEOUT)` on the
    /// request killed every direct SSE stream at 3s, before the fallback
    /// could ever matter. A stalled body on this client is bounded by the
    /// SSE idle-read timeout instead (`idle_timeout`).
    direct_client: reqwest::Client,
    tailscale: Arc<TailscaleClient>,
    /// SSE idle-read timeout — if no bytes arrive for this long, the stream
    /// is terminated with a transient error (see `parse_openai_sse`). Per
    /// provider, from the config blob's `idle_timeout_secs`, default 120s.
    idle_timeout: Duration,
}

/// Bounds time-to-response-headers on a chat completion. `connect_timeout`
/// only covers TCP/TLS setup — a provider that accepts the connection but
/// stalls before sending response headers would otherwise hold the turn
/// (and the fallback loop) forever. The SSE *body* is never capped by this;
/// that's the per-chunk `idle_timeout`'s job.
const RESPONSE_HEADERS_TIMEOUT: Duration = Duration::from_secs(30);

/// Bounds time-to-headers for the DIRECT Tailscale attempt only. The direct
/// path is an optimization over the tunnel — a half-open/stale LAN address
/// must not burn the full `RESPONSE_HEADERS_TIMEOUT` before falling back.
/// Like the outer wrapper, this resolves as soon as headers arrive and never
/// caps a slow-but-healthy SSE *body* (that's `idle_timeout`'s job).
const DIRECT_HEADERS_TIMEOUT: Duration = Duration::from_secs(5);

/// TCP keepalive probe interval for provider connections (see #10). A
/// dead-but-open TCP connection (peer gone, NAT entry flushed, Android
/// backgrounding the app) used to be detectable only by the SSE idle-read
/// timeout — up to a full `idle_timeout` of stuck silence. Periodic
/// keepalives let the OS time the dead peer out within a few probes and
/// surface it as an ordinary connection error.
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Total-duration ceiling for one streamed response (see #10). The idle-read
/// timeout only bounds *gaps* between bytes — a provider dribbling data fast
/// enough to stay under the idle gap would otherwise hold a turn (and its
/// memory) open indefinitely. A turn cannot legitimately need an hour of
/// continuous streaming; if it does, `idle_timeout_secs` is the knob.
const MAX_STREAM_DURATION: Duration = Duration::from_secs(3600);

impl OpenAICompatibleProvider {
    pub const DEFAULT_MODEL: &'static str = "gpt-4o";

    pub fn new(provider_id: &str, config: ProviderConfig, tailscale: Arc<TailscaleClient>) -> Self {
        let idle_timeout = config.idle_timeout();
        let client = match reqwest::Client::builder()
            // Bound the TCP/TLS setup phase so a provider that accepts the
            // connection but stalls before sending response headers can't
            // block `chat_completion` forever. This does NOT bound the SSE
            // body (a healthy long stream would trip a whole-request timeout
            // — that's the per-chunk `idle_timeout`'s job instead).
            .connect_timeout(std::time::Duration::from_secs(30))
            .tcp_keepalive(TCP_KEEPALIVE_INTERVAL)
            .build()
        {
            Ok(c) => c,
            // Don't degrade silently (see #12): a builder failure must not
            // quietly hand a provider a default client without its
            // connect-timeout/keepalive settings — log it loudly. The
            // provider still constructs (the default client usually works),
            // so this is an error, not a fatal.
            Err(e) => {
                tracing::error!(
                    provider_id = %provider_id,
                    error = %e,
                    "failed to build the provider HTTP client; falling back to a default client (no connect-timeout/keepalive)"
                );
                reqwest::Client::new()
            }
        };
        let direct_client = match reqwest::Client::builder()
            .connect_timeout(crate::network::DIRECT_CONNECT_TIMEOUT)
            .tcp_keepalive(TCP_KEEPALIVE_INTERVAL)
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    provider_id = %provider_id,
                    error = %e,
                    "failed to build the Tailscale direct HTTP client; falling back to a default client (no connect-timeout/keepalive)"
                );
                reqwest::Client::new()
            }
        };
        Self {
            provider_id: provider_id.into(),
            client,
            direct_client,
            config,
            tailscale,
            idle_timeout,
        }
    }


    /// Merges the contiguous *leading* run of `role: "system"` messages into a
    /// single one at the front of the array, leaving everything else (and its
    /// relative order) untouched.
    ///
    /// Context building layers up to 5 separate system messages at the head
    /// (persona, session override, writable-dir hint, anchored first message,
    /// consolidated memory — see `agent/context/builder.rs`), plus an
    /// occasional *mid-conversation* one from `emergency_trim` / the budget
    /// loop. Many chat templates only special-case `messages[0]` (and
    /// sometimes `[1]`) as a mergeable leading system turn and silently drop
    /// any system-role message beyond that position instead of rendering it —
    /// observed on Qwen's official ChatML/tool-calling template, whose
    /// `num_sys` never exceeds 2. Merging the leading run keeps the front
    /// block at `num_sys == 1` (within that limit) while the non-leading
    /// system messages stay rendered.
    ///
    /// Only the leading run is merged: hoisting *every* system message to the
    /// front (the old behavior) moved per-turn tail hints / mid-conversation
    /// markers out of their in-stream position, which defeats KV-prefix
    /// caching for backends keyed on a stable prompt head. Mirrors what
    /// `AnthropicProvider::chat_completion` does for its single `system`
    /// string field.
    fn merge_system_messages(messages: Vec<Value>) -> Vec<Value> {
        let mut out = Vec::new();
        let mut iter = messages.into_iter();

        let mut system_parts: Vec<String> = Vec::new();
        // Consume the contiguous leading system run only; stop at the first
        // non-system message and keep processing the rest verbatim below.
        loop {
            match iter.next() {
                Some(msg) if msg["role"] == "system" => {
                    if let Some(c) = msg["content"].as_str() {
                        system_parts.push(c.to_string());
                    }
                }
                Some(msg) => {
                    out.push(msg);
                    break;
                }
                None => break,
            }
        }

        if !system_parts.is_empty() {
            out.insert(0, serde_json::json!({
                "role": "system",
                "content": system_parts.join("\n\n"),
            }));
        }
        out.extend(iter);
        out
    }

    /// Move *every* system message (not just the leading run) into a single
    /// leading system block, preserving order, and keep all non-system messages
    /// in their relative order. For chat templates that require the system
    /// message to be first (llama-server's Qwen3, some Ollama templates), which
    /// raise on a tail/mid-stream system message. Unlike `merge_system_messages`
    /// (leading-run only, KV-cache-preserving) this sacrifices the tail
    /// placement of injected recall to satisfy the template — correctness over
    /// prefix-cache reuse, and only for the self-hosted dialects that need it.
    fn hoist_all_system_messages(messages: Vec<Value>) -> Vec<Value> {
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
        let mut out = Vec::new();
        if !system_parts.is_empty() {
            out.push(serde_json::json!({
                "role": "system",
                "content": system_parts.join("\n\n"),
            }));
        }
        out.extend(rest);
        out
    }

    /// Convert `tool_calls[].function.arguments` in assistant messages to the
    /// JSON string the OpenAI-compatible wire format requires.
    ///
    /// Internally the agent loop carries tool-call arguments as a parsed JSON
    /// `Value` (object) everywhere — the provider stream fragments parse the
    /// arguments text into a `Value` (`parse_openai_sse`'s tool-call flush),
    /// `build_assistant_message` persists it as-is, and history reloaded from
    /// the DB has the same shape. The OpenAI/OpenRouter/Azure chat-completions
    /// schema, however, insists `function.arguments` is a JSON **string** —
    /// sending an object yields:
    ///
    /// `Invalid type for 'input[3].arguments': expected a string, but got an
    /// object instead.` (400, `invalid_type`) — observed via OpenRouter
    /// fanning out to Azure/OpenAI once a session has one tool-calling turn
    /// in its history.
    ///
    /// This runs at send time so every message source is covered (fresh
    /// turn, reloaded history, compaction survivors) regardless of how it
    /// was built; already-string arguments (some backends stream a string
    /// and our parse would have normalized them, but third-party-built
    /// messages can arrive string-shaped) are left untouched. The Anthropic
    /// provider is deliberately unaffected: its `convert_tool_calls` maps
    /// `arguments` to the `input` field, which Anthropic expects as an object.
    fn stringify_tool_call_arguments(messages: Vec<Value>) -> Vec<Value> {
        messages
            .into_iter()
            .map(|mut msg| {
                if msg["role"] != "assistant" {
                    return msg;
                }
                let Some(tool_calls) = msg.get_mut("tool_calls").and_then(|v| v.as_array_mut())
                else {
                    return msg;
                };
                for tc in tool_calls.iter_mut() {
                    let Some(args) = tc
                        .get_mut("function")
                        .and_then(|f| f.as_object_mut())
                        .and_then(|f| f.get_mut("arguments"))
                    else {
                        continue;
                    };
                    if args.is_string() {
                        continue;
                    }
                    *args = Value::String(serde_json::to_string(args).unwrap_or_else(
                        |_| "{}".to_string(),
                    ));
                }
                msg
            })
            .collect()
    }

    /// Single direct attempt against `direct_url`, bounded by
    /// `DIRECT_HEADERS_TIMEOUT`. Returns `Err` on timeout, transport error,
    /// or any non-success status — a stale direct address answering `401`/`500`
    /// is *not* a usable response (the tunnel is the authoritative path).
    /// `send()` resolves as soon as headers arrive, so the timeout never caps
    /// a slow-but-healthy SSE *body* (that's `idle_timeout`'s job).
    async fn try_direct(
        &self,
        direct_url: &str,
        body: &Value,
    ) -> Result<reqwest::Response, ()> {
        let direct = self
            .direct_client
            .post(direct_url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .json(body)
            .send();
        match tokio::time::timeout(DIRECT_HEADERS_TIMEOUT, direct).await {
            Ok(Ok(resp)) if resp.status().is_success() => Ok(resp),
            _ => Err(()),
        }
    }

    /// If `url`'s host is a Tailscale peer with a discoverable direct (LAN)
    /// address, tries that address first (via `try_direct`: connect +
    /// time-to-headers bounded by `DIRECT_HEADERS_TIMEOUT`) and falls back to
    /// the original (tunneled) URL on timeout, transport error, or a
    /// non-success response. A stale direct IP answering `401`/`500` is *not*
    /// a usable response — the tunnel is the authoritative path, so surfacing
    /// a bogus endpoint failure instead of falling back used to kill every
    /// Tailscale-provider turn after a network change. A no-op — single
    /// request, original URL — for every other host (localhost,
    /// non-Tailscale, or no direct address known). Mirrors Python's
    /// `PreferDirectTransport`.
    async fn send_preferring_direct(
        &self,
        url: &str,
        body: &Value,
    ) -> Result<reqwest::Response, reqwest::Error> {
        if let Some(direct_url) = maybe_direct_url(&self.tailscale, url).await {
            if let Ok(resp) = self.try_direct(&direct_url, body).await {
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

    /// Unlike Anthropic, whether a trailing partial assistant message
    /// actually continues generation (rather than erroring, or the server
    /// just starting a fresh turn and ignoring it) depends on the specific
    /// OpenAI-compatible server/chat-template combination and isn't part of
    /// the spec -- see `ProviderConfig::experimental_prefill`'s doc comment.
    /// Explicit per-provider opt-in only; never assumed.
    fn supports_assistant_prefill(&self) -> bool {
        self.config.experimental_prefill
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
        // Self-hosted chat templates (llama.cpp `llama-server`'s Qwen3, some
        // Ollama templates) raise "System message must be at the beginning of
        // conversation" on any system message that isn't first — but the
        // context builder injects memory/recall as system blocks in the *tail*
        // (kept there for KV-prefix caching on providers that tolerate it, and
        // surfaced after a stop+followup once recall has content). Hoist every
        // system message to a single leading block for these dialects; hosted
        // OpenAI/OpenRouter tolerate mid-stream system messages, so they keep
        // the cache-preserving leading-run-only merge.
        let messages = if matches!(
            self.config.provider_type.as_str(),
            "ollama" | "custom_openai"
        ) {
            Self::hoist_all_system_messages(messages)
        } else {
            Self::merge_system_messages(messages)
        };
        // History can carry tool-call arguments as objects (built this way
        // throughout the daemon) but OpenAI-compatible backends require them
        // as JSON strings — normalize before serializing the body.
        let messages = Self::stringify_tool_call_arguments(messages);

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
        // Clamp max_tokens to a sane range (mirror of anthropic.rs): a
        // configured 0/negative or absurd value produces an opaque provider
        // 400 on the turn.
        if let Some(m) = sampling.max_tokens {
            if (1..=65536).contains(&m) {
                body["max_tokens"] = m.into();
            }
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
        // Reasoning effort — dialect-specific. OpenAI takes a flat
        // `reasoning_effort` string (with no way to switch reasoning *off* on
        // an o-series model — "Off" there just means "send nothing"), while
        // OpenRouter takes a nested `reasoning` object (and *can* disable it).
        // A self-hosted OpenAI-compatible server (llama.cpp `llama-server`,
        // Ollama) honors neither of those; the portable control there is the
        // chat template's `enable_thinking` kwarg (Qwen3 et al.), so effort
        // collapses to on/off and rides `chat_template_kwargs`. `local` (the
        // in-process engine) still has no equivalent and writes nothing.
        if let Some(e) = sampling.effort.as_ref() {
            match self.config.provider_type.as_str() {
                "openai" => {
                    if let Some(s) = e.hosted_level() {
                        body["reasoning_effort"] = s.into();
                    }
                }
                "openrouter" => {
                    body["reasoning"] = openrouter_reasoning(e);
                }
                "custom_openai" | "ollama" => {
                    // A self-hosted server exposes reasoning several ways and
                    // different builds honor different ones, so populate all and
                    // let the server use whichever it understands:
                    //   * `chat_template_kwargs.enable_thinking` — Qwen3 et al.
                    //     (a boolean on/off toggle baked into the chat template).
                    //   * `chat_template_kwargs.reasoning_effort` — the graded
                    //     level the template reads as a variable
                    //     (`reasoning_effort|default('xhigh')`). This is the
                    //     value Kitty discovered from the server's own template
                    //     (`bigtiny::effort`), so it is passed **verbatim** —
                    //     `xhigh`/`medium`/`low`, whatever the model declared,
                    //     never clamped to a fixed set.
                    //   * top-level `reasoning_effort` — gpt-oss / recent
                    //     llama-server builds that read the OpenAI-style field.
                    // Off → thinking disabled and no effort field.
                    let enable = !matches!(e, Effort::Off);
                    let kwargs = body
                        .get_mut("chat_template_kwargs")
                        .and_then(|v| v.as_object_mut());
                    let obj = match kwargs {
                        Some(obj) => obj,
                        None => {
                            body["chat_template_kwargs"] = serde_json::json!({});
                            body["chat_template_kwargs"].as_object_mut().unwrap()
                        }
                    };
                    obj.insert("enable_thinking".into(), enable.into());
                    if let Some(level) = e.wire_level() {
                        obj.insert("reasoning_effort".into(), level.into());
                        body["reasoning_effort"] = level.into();
                    }
                }
                _ => {}
            }
        }
        if let Some(slot) = id_slot {
            body["id_slot"] = slot.into();
        }

        // Never log the request body itself: it embeds the full conversation
        // (system prompt, history, tool output) at debug level. A shape
        // summary is enough to correlate a request in the logs.
        tracing::debug!(
            provider_id = %self.provider_id,
            model = %model,
            messages = body["messages"].as_array().map(|m| m.len()).unwrap_or(0),
            tools = body["tools"].as_array().map(|t| t.len()).unwrap_or(0),
            "chat_completion request"
        );

        // Bound time-to-response-headers (see RESPONSE_HEADERS_TIMEOUT);
        // `send().await` resolves as soon as headers arrive, so this never
        // caps the SSE body streaming behind them.
        let resp = tokio::time::timeout(
            RESPONSE_HEADERS_TIMEOUT,
            self.send_preferring_direct(&url, &body),
        )
        .await
        .map_err(|_| ProviderError::Timeout {
            user_message: format!(
                "Provider sent no response headers within {}s",
                RESPONSE_HEADERS_TIMEOUT.as_secs()
            ),
            raw_message: "timed out waiting for response headers".into(),
            http_status: 0,
        })?
        .map_err(|e| classify_transport_error(&e, format!("failed to reach provider: {e}")))?;

        let status_code = resp.status().as_u16();
        if !resp.status().is_success() {
            // `Retry-After` (429/503) must be read before the body is
            // consumed — the retry loop honors it as a backoff floor.
            let retry_after = parse_retry_after(resp.headers());
            // Bounded in both time and size — a stalled error body used to
            // hang the turn forever (see `read_bounded_error_body`).
            let body_text = read_bounded_error_body(resp).await;
            return Err(classify_provider_error(status_code, &body_text, retry_after));
        }

        // Use bytes_stream from the stream feature
        let stream = resp.bytes_stream();
        let deltas = parse_openai_sse(stream, self.idle_timeout);
        Ok(Box::pin(deltas))
    }

    async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let url = format!("{}/v1/models", self.config.base_url);
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            // Same per-request bound as `check_health` — a stalled provider
            // must not hang model discovery forever.
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| {
                classify_transport_error(&e, format!("failed to discover models: {e}"))
            })?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(classify_provider_error(0, &body, None));
        }

        let data: Value = resp.json().await.map_err(|e| ProviderError::Other {
            user_message: format!("Failed to parse models response: {}", e),
            raw_message: e.to_string(),
            http_status: 0,
            retry_after_secs: None,
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

/// Cap on the line buffer between newlines — a broken/hostile provider
/// streaming garbage with no newlines used to grow `buf` without limit (OOM
/// vector, see #7). Any line past this is terminated as a transient error.
/// Chosen far above any legitimate single JSON SSE frame.
const MAX_SSE_LINE_BYTES: usize = 8 * 1024 * 1024;
/// Cap on accumulated streamed tool-argument text per tool call — the old
/// code grew `PendingToolCall::arguments` unboundedly. Same reasoning as
/// `MAX_SSE_LINE_BYTES`; 1MB is far beyond any realistic tool signature.
const MAX_TOOL_ARGUMENTS_BYTES: usize = 1024 * 1024;

type RawBytesStream = Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>;

struct OpenAISSEStream {
    /// `tokio_stream::adapters::Timeout` wraps the raw bytes stream: `Item =
    /// Result<Result<Bytes, reqwest::Error>, Elapsed>` — the idle-read
    /// timeout (per-provider `idle_timeout`) resets on every arriving chunk
    /// and fires one `Err(Elapsed)` when nothing has arrived for that long. A
    /// long, actively-streaming turn is never capped.
    inner: Pin<Box<tokio_stream::adapters::Timeout<RawBytesStream>>>,
    tool_call_buf: HashMap<usize, PendingToolCall>,
    /// Raw bytes between newlines, decoded to UTF-8 *only* at complete line
    /// boundaries — decoding per TCP chunk (`String::from_utf8_lossy`) used to
    /// corrupt a multi-byte UTF-8 character split across two chunks.
    buf: Vec<u8>,
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
    /// Carries `<think>`/`</think>` state across `process_line` calls (i.e.
    /// across SSE chunks), since a single reasoning block routinely spans
    /// many deltas.
    thinking: TagSplitter,
    /// When this stream's response started (see `MAX_STREAM_DURATION`, #10).
    stream_started: Instant,
}

fn parse_openai_sse(
    stream: impl Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    idle_timeout: Duration,
) -> OpenAISSEStream {
    use tokio_stream::StreamExt as _;
    let inner = Box::pin(stream)
        as Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>;
    let inner = inner.timeout(idle_timeout);
    OpenAISSEStream {
        inner: Box::pin(inner),
        tool_call_buf: HashMap::new(),
        buf: Vec::new(),
        pending: std::collections::VecDeque::new(),
        done: false,
        last_finish_reason: None,
        thinking: TagSplitter::thinking(),
        stream_started: Instant::now(),
    }
}

impl OpenAISSEStream {
    /// Split `<think>...</think>` out of one content fragment. The
    /// cross-fragment bookkeeping lives in `TagSplitter` — a tag spanning
    /// several SSE deltas is the normal case, not the exception.
    fn split_thinking_tags(&mut self, content: &str) -> (String, Option<String>) {
        let split = self.thinking.feed(content);
        let reasoning = if split.inside.is_empty() {
            None
        } else {
            Some(split.inside)
        };
        (split.outside, reasoning)
    }
    /// Flush any accumulated streamed tool calls as a single Delta. Shared by
    /// the `[DONE]` path and the `Poll::Ready(None)` path so a stream that
    /// ends *without* the `[DONE]` marker (some backends just close the
    /// connection) still surfaces its complete tool calls instead of silently
    /// dropping them.
    fn flush_tool_call_buf(&mut self) {
        if self.tool_call_buf.is_empty() {
            return;
        }
        // HashMap iteration order is nondeterministic — sort by the streamed
        // `index` so tool calls execute and transcribe in the order the model
        // emitted them, not in hash order.
        let mut entries: Vec<(usize, PendingToolCall)> = self.tool_call_buf.drain().collect();
        entries.sort_by_key(|(idx, _)| *idx);
        let bad: Vec<String> = entries
            .iter()
            .filter(|(_, buf)| {
                // Empty accumulated arguments are legitimate (a zero-arg tool
                // streams no fragments) — only NON-empty unparseable text is
                // malformed.
                !buf.arguments.is_empty()
                    && serde_json::from_str::<Value>(&buf.arguments).is_err()
            })
            .map(|(_, buf)| buf.name.clone().unwrap_or_else(|| "<unnamed>".into()))
            .collect();
        let tool_calls: Vec<super::base::ToolCall> = entries
            .into_iter()
            .map(|(_, buf)| {
                // `function` must carry BOTH `name` and `arguments` — the
                // agent loop reads `tc.function.get("name")` /
                // `.get("arguments")`. The previous version set
                // `function` to just the parsed arguments object with no
                // `name` key at all (and never captured the streamed
                // `function.name` fragment in the first place), so every
                // tool call executed as an unnamed/"unknown" tool.
                //
                // Malformed (truncated/incomplete) arguments must NOT be
                // silently replaced with `{}` — that executes the tool with
                // empty args (a `read_file` becomes `read_file(path:
                // undefined)`), corrupting user-visible effects. Emit an
                // error delta so the agent loop surfaces a failed call
                // instead. EMPTY accumulated arguments are the opposite
                // case: a zero-argument tool streams no `arguments`
                // fragments at all, so `""` legitimately means `{}` (the
                // Anthropic parser already treats it that way).
                let arguments: Value = if buf.arguments.is_empty() {
                    serde_json::json!({})
                } else {
                    match serde_json::from_str(&buf.arguments) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(
                                "tool call {:?} had unparseable streamed arguments: {e}",
                                buf.name
                            );
                            serde_json::json!({ "__error": format!("malformed tool arguments: {e}") })
                        }
                    }
                };
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
        // Only emit extra error deltas when there was an actual parse failure —
        // the well-formed calls still flow as normal tool_calls below.
        if !bad.is_empty() {
            self.pending.push_back(Delta {
                role: "assistant".into(),
                content: None,
                reasoning: None,
                tool_calls: None,
                finish_reason: None,
                usage: None,
                error_type: Some(
                    format!(
                        "malformed streamed tool arguments for: {}",
                        bad.join(", ")
                    ),
                ),
            });
        }
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

    /// Process one complete SSE line, pushing any resulting Delta(s) onto `pending`.
    /// Returns true if this line signalled stream completion (`[DONE]`).
    fn process_line(&mut self, line: &str) -> bool {
        // Per the SSE spec the colon may be followed by zero-or-more spaces —
        // many llama.cpp-era servers emit `data:{...}` with no space, and a
        // strict `data: ` would silently drop those lines (including a
        // lost `[DONE]`, hanging the turn until the idle timeout).
        let trimmed = line.trim_start();
        let Some(data) = trimmed.strip_prefix("data:").map(|d| d.trim_start()) else {
            return false;
        };

        if data == "[DONE]" {
            self.flush_tool_call_buf();
            return true;
        }

        let json: Value = match serde_json::from_str(data) {
            Ok(j) => j,
            Err(e) => {
                // Previously dropped silently — a malformed chunk (truncated
                // JSON, non-JSON interleave) then produced no signal at all
                // and the turn ran on until the idle timeout. Log it so the
                // failure is visible in daemon logs.
                tracing::debug!("dropped malformed SSE data line: {e}: {data}");
                return false;
            }
        };

        // A top-level `error` object (many OpenAI-compatible endpoints emit
        // this on mid-stream failures instead of a `choices` chunk) was
        // previously swallowed — the stream then ended with no finish reason
        // at all, feeding the unbounded step-retry path in `agent::loop_`
        // (see #1). Surface it as the same transient-error delta as a dropped
        // connection so `process_stream` routes it through retry/failover.
        if json.get("error").is_some() {
            self.pending.push_back(Delta {
                role: "assistant".into(),
                content: None,
                reasoning: None,
                tool_calls: None,
                finish_reason: Some("error".into()),
                usage: None,
                error_type: Some("request".into()),
            });
            return true;
        }

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
                            if entry.arguments.len() + f.len() > MAX_TOOL_ARGUMENTS_BYTES {
                                // Unbounded per-tool accumulation was an OOM
                                // vector (see #7) — a broken provider
                                // streaming ever-growing argument JSON used to
                                // grow `arguments` forever. Terminate as a
                                // transient error so the turn routes through
                                // retry/failover.
                                self.pending.push_back(Delta {
                                    role: "assistant".into(),
                                    content: None,
                                    reasoning: None,
                                    tool_calls: None,
                                    finish_reason: Some("error".into()),
                                    usage: None,
                                    error_type: Some("request".into()),
                                });
                                return true;
                            }
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
            // Total-stream-duration cap (see #10, `MAX_STREAM_DURATION`): the
            // idle-read timeout only bounds gaps between bytes — a provider
            // dribbling data fast enough to beat it must not hold this turn
            // (and its memory) open forever. Buffered content already queued
            // buffered in `pending` are drained first (the check sits below
            // the drain), then the turn gets the usual transient error.
            if self.stream_started.elapsed() >= MAX_STREAM_DURATION {
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
                continue;
            }

            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(Ok(chunk)))) => {
                    self.buf.extend_from_slice(&chunk);
                    // Unbounded line buffer was an OOM vector (see #7): a
                    // provider streaming garbage without newlines grew `buf`
                    // forever. Any line past the cap is a broken/hostile
                    // stream — terminate with the transient-error delta.
                    if self.buf.len() > MAX_SSE_LINE_BYTES {
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
                        continue;
                    }
                    while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                        let mut raw: Vec<u8> = self.buf.drain(..=pos).collect();
                        while matches!(raw.last(), Some(&b'\r') | Some(&b'\n')) {
                            raw.pop();
                        }
                        // Decode only at complete line boundaries — this is
                        // what keeps a multi-byte UTF-8 char split across two
                        // TCP chunks intact (see `buf`'s comment).
                        let line = String::from_utf8(raw).unwrap_or_else(|e| {
                            String::from_utf8_lossy(e.as_bytes()).into_owned()
                        });
                        if self.process_line(&line) {
                            self.done = true;
                        }
                    }
                    // Loop back: drain anything just queued, or poll inner again if
                    // this chunk had no complete `data:` line yet.
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
                // The idle-read timeout fired: no bytes arrived within
                // `idle_timeout`. Surface it with the same transient-error
                // shape as an ordinary stream error so `agent::loop_` treats
                // an idle/stuck provider as a transient failure rather than
                // hanging the turn.
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
                    // Release any held-back partial `<think>` tag as literal
                    // text — the entire reason `TagSplitter::flush` exists.
                    // A stream ending in a dangling `<thi` used to drop those
                    // bytes silently. Inside a think span the tail is
                    // reasoning; outside, ordinary content.
                    let think_tail = self.thinking.flush();
                    if !think_tail.is_empty() {
                        let (content, reasoning) = if self.thinking.is_inside() {
                            (None, Some(think_tail))
                        } else {
                            (Some(think_tail), None)
                        };
                        self.pending.push_back(Delta {
                            role: "assistant".into(),
                            content,
                            reasoning,
                            tool_calls: None,
                            finish_reason: None,
                            usage: None,
                            error_type: None,
                        });
                    }
                    // Flush any accumulated tool calls: some backends close
                    // the stream without an explicit `[DONE]` marker, and
                    // complete tool-call fragments would otherwise be lost.
                    self.flush_tool_call_buf();
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
mod effort_tests {
    use super::*;

    #[test]
    fn hosted_level_maps_levels_and_omits_off() {
        assert_eq!(Effort::Low.hosted_level(), Some("low"));
        assert_eq!(Effort::Medium.hosted_level(), Some("medium"));
        assert_eq!(Effort::High.hosted_level(), Some("high"));
        // No way to disable o-series reasoning — "off" is encoded as omission.
        assert_eq!(Effort::Off.hosted_level(), None);
        // A model-specific level (Qwen3's xhigh) clamps to the hosted ceiling.
        assert_eq!(Effort::Custom("xhigh".into()).hosted_level(), Some("high"));
    }

    /// The self-hosted wire path forwards the level verbatim (not clamped), so
    /// a Qwen3 template's `reasoning_effort` variable gets the real `xhigh`.
    #[test]
    fn wire_level_is_verbatim_for_self_hosted() {
        assert_eq!(Effort::Custom("xhigh".into()).wire_level(), Some("xhigh"));
        assert_eq!(Effort::Medium.wire_level(), Some("medium"));
        assert_eq!(Effort::Off.wire_level(), None);
    }

    #[test]
    fn openrouter_uses_a_nested_object_and_can_disable() {
        assert_eq!(
            openrouter_reasoning(&Effort::Off),
            serde_json::json!({ "enabled": false })
        );
        assert_eq!(
            openrouter_reasoning(&Effort::High),
            serde_json::json!({ "effort": "high" })
        );
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
        let deltas: Vec<Delta> = parse_openai_sse(inner, Duration::from_secs(300)).collect().await;
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
        let deltas: Vec<Delta> = parse_openai_sse(inner, Duration::from_secs(300)).collect().await;
        let contents: Vec<String> = deltas.into_iter().filter_map(|d| d.content).collect();
        assert_eq!(contents, vec!["Hello".to_string()]);
    }

    #[tokio::test]
    async fn multi_byte_utf8_split_across_chunks_round_trips() {
        // "café" — the é (U+00E9 = UTF-8 0xC3 0xA9) is split across the two
        // chunks: chunk1 ends with the leading 0xC3 byte, chunk2 begins with
        // 0xA9. The old per-chunk `String::from_utf8_lossy` decoded 0xC3 in
        // isolation → replacement char, corrupting the text wherever a
        // multi-byte char straddled a TCP chunk boundary.
        let mut chunk1 = b"data: {\"choices\":[{\"delta\":{\"content\":\"caf".to_vec();
        chunk1.push(0xC3);
        let chunk2 = b"\xA9\"}}]}\n\ndata: [DONE]\n\n".to_vec();
        let inner = stream::iter(vec![
            Ok::<bytes::Bytes, reqwest::Error>(bytes::Bytes::from(chunk1)),
            Ok::<bytes::Bytes, reqwest::Error>(bytes::Bytes::from(chunk2)),
        ]);
        let deltas: Vec<Delta> = parse_openai_sse(inner, Duration::from_secs(300))
            .collect()
            .await;
        let contents: String = deltas.iter().filter_map(|d| d.content.clone()).collect();
        assert_eq!(contents, "café");
    }

    #[tokio::test]
    async fn complete_tool_calls_are_flushed_when_the_stream_ends_without_done() {
        // Some backends close the connection without ever sending `[DONE]` —
        // accumulated tool-call fragments used to be silently dropped on that
        // path. Flush must surface the complete tool call.
        let chunk = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}]}}]}\n\n";
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_openai_sse(inner, Duration::from_secs(300))
            .collect()
            .await;
        let tool_calls = deltas
            .into_iter()
            .find_map(|d| d.tool_calls)
            .expect("expected a Delta carrying tool_calls");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_1");
        assert_eq!(
            tool_calls[0]
                .function
                .get("name")
                .and_then(|v| v.as_str()),
            Some("read_file")
        );
    }

    #[tokio::test]
    async fn idle_read_timeout_terminates_a_silent_stream_as_a_transient_error() {
        // A stream that never yields bytes: once `idle_timeout` elapses the
        // wrapper fires, and we must surface the same transient-error Delta as
        // the ordinary stream-error path (agent::loop_ treats that as a
        // retryable failure rather than hanging the turn). A long,
        // actively-streaming turn is never capped — only idle gaps are.
        let silent = stream::pending::<Result<bytes::Bytes, reqwest::Error>>();
        let deltas: Vec<Delta> = parse_openai_sse(silent, Duration::from_millis(50))
            .collect()
            .await;
        let last = deltas.last().expect("expected at least the error Delta");
        assert_eq!(last.error_type.as_deref(), Some("request"));
        assert_eq!(last.finish_reason.as_deref(), Some("error"));
    }

    #[tokio::test]
    async fn top_level_error_object_surfaces_as_a_transient_error_delta() {
        // An OpenAI-compatible endpoint can fail mid-stream with a top-level
        // `error` object instead of a `choices` chunk. That used to be
        // silently swallowed — the stream ended with no finish reason and the
        // turn ran on unboundedly. It must surface as the same transient-error
        // delta as a dropped connection so `process_stream` retries/fails over.
        let chunk =
            "data: {\"error\": {\"message\": \"upstream failure\", \"type\": \"server_error\"}}\n\n";
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_openai_sse(inner, Duration::from_secs(300))
            .collect()
            .await;
        let last = deltas.last().expect("expected the error Delta");
        assert_eq!(last.error_type.as_deref(), Some("request"));
        assert_eq!(last.finish_reason.as_deref(), Some("error"));
    }

    /// #7 regression: a provider streaming garbage with no newlines must not
    /// grow the line buffer without bound — past `MAX_SSE_LINE_BYTES` the
    /// stream terminates with the transient-error delta instead of OOMing.
    #[tokio::test]
    async fn an_overlong_line_without_newline_terminates_as_a_transient_error() {
        let garbage = vec![b'x'; MAX_SSE_LINE_BYTES + 1];
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(bytes::Bytes::from(
            garbage,
        ))]);
        let deltas: Vec<Delta> = parse_openai_sse(inner, Duration::from_secs(300))
            .collect()
            .await;
        assert_eq!(deltas.len(), 1, "exactly one transient-error delta");
        assert_eq!(deltas[0].error_type.as_deref(), Some("request"));
        assert_eq!(deltas[0].finish_reason.as_deref(), Some("error"));
    }

/// #7 regression: unbounded per-tool argument accumulation must terminate
    /// as a transient error rather than growing memory forever.
    #[tokio::test]
    async fn overlong_tool_arguments_terminate_as_a_transient_error() {
        let payload = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "boom",
                            "arguments": "x".repeat(MAX_TOOL_ARGUMENTS_BYTES + 1),
                        },
                    }],
                },
            }],
        });
        let chunk = format!("data: {payload}\n\n");
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_openai_sse(inner, Duration::from_secs(300))
            .collect()
            .await;
        let last = deltas.last().expect("expected the transient-error Delta");
        assert_eq!(last.error_type.as_deref(), Some("request"));
        assert_eq!(last.finish_reason.as_deref(), Some("error"));
    }

    /// #10 regression: a stream open longer than `MAX_STREAM_DURATION` must
    /// terminate as a transient error instead of waiting on a provider that
    /// keeps just enough bytes flowing to beat the idle-read timeout.
    #[tokio::test]
    async fn a_stream_past_the_total_duration_cap_terminates_as_a_transient_error() {
        let silent = stream::pending::<Result<bytes::Bytes, reqwest::Error>>();
        let mut s = parse_openai_sse(silent, Duration::from_secs(300));
        s.stream_started = Instant::now() - (MAX_STREAM_DURATION + Duration::from_secs(1));
        let deltas: Vec<Delta> = s.collect().await;
        let last = deltas.last().expect("expected the transient-error Delta");
        assert_eq!(last.error_type.as_deref(), Some("request"));
        assert_eq!(last.finish_reason.as_deref(), Some("error"));
    }

    #[tokio::test]
    async fn done_marker_ends_stream() {
        let chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\ndata: [DONE]\n\n";
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_openai_sse(inner, Duration::from_secs(300)).collect().await;
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].content.as_deref(), Some("Hi"));
    }

    #[tokio::test]
    async fn trailing_complete_line_without_newline_is_still_decoded() {
        // A final `data:` line with no trailing `\n` is still a real SSE
        // line — it must be decoded and surfaced at end-of-stream rather than
        // dropped with the buffer.
        let chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}";
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_openai_sse(inner, Duration::from_secs(300)).collect().await;
        let contents: Vec<String> = deltas.into_iter().filter_map(|d| d.content).collect();
        assert_eq!(contents, vec!["Hi".to_string()]);
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
        let deltas: Vec<Delta> = parse_openai_sse(inner, Duration::from_secs(300)).collect().await;
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

    /// Regression: a zero-argument tool streams NO `arguments` fragments at
    /// all, leaving the accumulated string empty. `""` used to hit the JSON
    /// parse error path and surface as the `__error` sentinel — but empty
    /// arguments legitimately mean `{}` (the Anthropic parser already treats
    /// them that way).
    #[tokio::test]
    async fn zero_argument_tool_call_flushes_as_empty_object_not_error() {
        let chunk = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"list_files\"}}]}}]}\n\n\
                     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                     data: [DONE]\n\n";
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_openai_sse(inner, Duration::from_secs(300)).collect().await;
        assert!(
            deltas.iter().all(|d| d.error_type.is_none()),
            "no error delta for a legitimate zero-arg call: {deltas:?}"
        );
        let tool_calls = deltas
            .into_iter()
            .find_map(|d| d.tool_calls)
            .expect("expected a Delta carrying tool_calls");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(
            tool_calls[0].function.get("arguments"),
            Some(&serde_json::json!({})),
            "empty streamed arguments must become an empty object"
        );
    }

    /// Regression: `tool_call_buf` is a HashMap, and draining it emitted
    /// tool calls in nondeterministic hash order — scrambling the
    /// transcript/execution sequence. They must come out sorted by the
    /// streamed `index`, regardless of fragment arrival order.
    #[tokio::test]
    async fn tool_calls_flush_sorted_by_index_not_arrival_order() {
        // Fragments for index 2 arrive before index 0; the flush must still
        // order the calls 0, 1, 2.
        let chunk = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":2,\"id\":\"call_c\",\"type\":\"function\",\"function\":{\"name\":\"third\",\"arguments\":\"{}\"}}]}}]}\n\n\
                     data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"first\",\"arguments\":\"{}\"}}]}}]}\n\n\
                     data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"type\":\"function\",\"function\":{\"name\":\"second\",\"arguments\":\"{}\"}}]}}]}\n\n\
                     data: [DONE]\n\n";
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_openai_sse(inner, Duration::from_secs(300)).collect().await;
        let tool_calls = deltas
            .into_iter()
            .find_map(|d| d.tool_calls)
            .expect("expected a Delta carrying tool_calls");
        let names: Vec<&str> = tool_calls
            .iter()
            .filter_map(|tc| tc.function.get("name").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(names, vec!["first", "second", "third"]);
    }

    /// Regression: `OpenAISSEStream` never called `TagSplitter::flush` at
    /// end-of-stream, so a stream ending in a dangling partial tag (`<th`)
    /// dropped those bytes silently. The tail must surface as a final
    /// content delta.
    #[tokio::test]
    async fn dangling_partial_think_tag_is_flushed_at_end_of_stream() {
        let chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"tail<th\"}}]}\n\n";
        let inner = stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from(chunk),
        )]);
        let deltas: Vec<Delta> = parse_openai_sse(inner, Duration::from_secs(300)).collect().await;
        let content: String = deltas.iter().filter_map(|d| d.content.clone()).collect();
        assert_eq!(content, "tail<th", "the held-back partial tag must be flushed");
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
        let deltas: Vec<Delta> = parse_openai_sse(inner, Duration::from_secs(300)).collect().await;
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
        let deltas: Vec<Delta> = parse_openai_sse(inner, Duration::from_secs(300)).collect().await;
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
        let deltas: Vec<Delta> = parse_openai_sse(inner, Duration::from_secs(300)).collect().await;
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
        let deltas: Vec<Delta> = parse_openai_sse(inner, Duration::from_secs(300)).collect().await;
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

    /// Regression for the request-level `.timeout(DIRECT_CONNECT_TIMEOUT)`
    /// on the direct attempt: a reqwest timeout covers the whole response
    /// body, so every direct/Tailscale SSE stream died at 3s and the
    /// fallback never got to matter. The direct attempt must bound only the
    /// connect phase; a slow-but-healthy body streaming PAST
    /// `DIRECT_CONNECT_TIMEOUT` must complete.
    ///
    /// Uses a raw TCP fake rather than mockito, which can't delay mid-body.
    #[tokio::test]
    async fn send_preferring_direct_does_not_cap_the_sse_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Read the request head (headers + start of body is fine — we
            // never validate it).
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
            sock.write_all(head.as_bytes()).await.unwrap();
            let chunk = |data: &str| format!("{:x}\r\n{}\r\n", data.len(), data);
            sock.write_all(
                chunk("data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n").as_bytes(),
            )
            .await
            .unwrap();
            sock.flush().await.unwrap();
            // Longer than DIRECT_CONNECT_TIMEOUT (3s): the old request-level
            // timeout killed the stream right here.
            tokio::time::sleep(Duration::from_millis(3400)).await;
            sock.write_all(chunk("data: [DONE]\n\n").as_bytes())
                .await
                .unwrap();
            sock.write_all(b"0\r\n\r\n").await.unwrap();
        });

        let tailscale = Arc::new(TailscaleClient::new());
        tailscale.seed_resolved_for_test("100.64.1.2", Some("127.0.0.1".to_string()));
        let provider = OpenAICompatibleProvider::new(
            "test",
            crate::config::ProviderConfig {
                // Host is a "Tailscale" address so the direct path engages;
                // the seeded direct address points at the fake above.
                base_url: format!("http://100.64.1.2:{port}"),
                ..Default::default()
            },
            tailscale,
        );
        let messages = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let stream = provider
            .chat_completion(messages, None, SamplingParams::default(), None, None)
            .await
            .unwrap();
        let deltas: Vec<Delta> = stream.collect().await;
        let contents: String = deltas.iter().filter_map(|d| d.content.clone()).collect();
        assert_eq!(contents, "Hi", "the body must stream past the old 3s cap");
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

    /// Regression for the #4 fix: a stale direct address that *answers* with a
    /// server error must not be surfaced as a usable response — before the
    /// fix, `send_preferring_direct` returned any `Ok(resp)` from the direct
    /// attempt, so a `500` from a leftover LAN IP after a Tailscale network
    /// change became a bogus endpoint failure that killed the turn even
    /// though the tunnel was fine. `try_direct` must reject non-success
    /// statuses (the tunnel is the authoritative path).
    #[tokio::test]
    async fn try_direct_rejects_a_server_error_from_a_stale_direct_address() {
        let mut stale = mockito::Server::new_with_opts_async(mockito::ServerOpts {
            host: "127.0.0.2",
            ..Default::default()
        })
        .await;
        let stale_mock = stale
            .mock("POST", "/v1/chat/completions")
            .with_status(500)
            .with_body("{\"error\":{\"message\":\"stale endpoint\"}}")
            .expect(1)
            .create_async()
            .await;

        let provider = OpenAICompatibleProvider::new(
            "test",
            crate::config::ProviderConfig::default(),
            Arc::new(TailscaleClient::new()),
        );
        let body = serde_json::json!({"messages": [{"role": "user", "content": "hi"}]});
        let direct_url = format!("{}/v1/chat/completions", stale.url());

        assert!(
            provider.try_direct(&direct_url, &body).await.is_err(),
            "a 500 from the direct path must not be treated as usable"
        );
        stale_mock.assert_async().await;
    }

    /// Same as above for a `401` from a stale direct address (the IP has been
    /// reused by some other service on the LAN). Must fall back to the tunnel,
    /// not surface the bogus auth failure.
    #[tokio::test]
    async fn try_direct_rejects_an_auth_error_from_a_stale_direct_address() {
        let mut stale = mockito::Server::new_with_opts_async(mockito::ServerOpts {
            host: "127.0.0.2",
            ..Default::default()
        })
        .await;
        let stale_mock = stale
            .mock("POST", "/v1/chat/completions")
            .with_status(401)
            .with_body("{\"error\":{\"message\":\"not your service\"}}")
            .expect(1)
            .create_async()
            .await;

        let provider = OpenAICompatibleProvider::new(
            "test",
            crate::config::ProviderConfig::default(),
            Arc::new(TailscaleClient::new()),
        );
        let body = serde_json::json!({"messages": [{"role": "user", "content": "hi"}]});
        let direct_url = format!("{}/v1/chat/completions", stale.url());

        assert!(
            provider.try_direct(&direct_url, &body).await.is_err(),
            "a 401 from the direct path must not be treated as usable"
        );
        stale_mock.assert_async().await;
    }

    /// Regression for the #4 fix: a half-open/stale direct address that
    /// accepts the TCP connection but never sends response headers must not
    /// block the turn for the full 30s `RESPONSE_HEADERS_TIMEOUT` — the
    /// direct attempt is an optimization and `try_direct` bounds it with
    /// `DIRECT_HEADERS_TIMEOUT`. The timeout fires around 5s and `try_direct`
    /// returns `Err`, letting the tunnel fallback proceed.
    #[tokio::test]
    async fn try_direct_bounds_a_silent_direct_address() {
        let listener = tokio::net::TcpListener::bind("127.0.0.2:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // Accept but never respond — simulates a half-open stale LAN
            // address whose TCP handshake succeeds but which never produces
            // response headers.
            let (_sock, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let provider = OpenAICompatibleProvider::new(
            "test",
            crate::config::ProviderConfig::default(),
            Arc::new(TailscaleClient::new()),
        );
        let body = serde_json::json!({"messages": [{"role": "user", "content": "hi"}]});
        let direct_url = format!("http://127.0.0.2:{port}/v1/chat/completions");

        let started = std::time::Instant::now();
        assert!(
            provider.try_direct(&direct_url, &body).await.is_err(),
            "a silent direct address must fail fast, not hang"
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_secs(5),
            "the direct attempt should ride out its full 5s header timeout"
        );
        assert!(
            elapsed < Duration::from_secs(8),
            "the direct attempt must not burn the 30s outer header timeout"
        );
    }

    #[test]
    fn merge_system_messages_joins_only_the_leading_system_run() {
        // Regression for the KV-prefix-cache breakage: the old code hoisted
        // *every* system message (including `memory`, below) to the front,
        // moving it out of its in-stream tail position. Only the contiguous
        // leading run (persona + tool hints) may be merged; the tail system
        // message must stay exactly where it was, unmerged.
        let messages = vec![
            serde_json::json!({"role": "system", "content": "persona"}),
            serde_json::json!({"role": "system", "content": "tool hints"}),
            serde_json::json!({"role": "user", "content": "hi"}),
            serde_json::json!({"role": "system", "content": "memory"}),
            serde_json::json!({"role": "assistant", "content": "hello"}),
        ];
        let merged = OpenAICompatibleProvider::merge_system_messages(messages);
        assert_eq!(merged.len(), 4);
        // Leading run merged into a single front system block...
        assert_eq!(merged[0]["role"], "system");
        assert_eq!(merged[0]["content"], "persona\n\ntool hints");
        // ...everything else keeps its original position and shape.
        assert_eq!(merged[1]["role"], "user");
        assert_eq!(merged[1]["content"], "hi");
        assert_eq!(merged[2]["role"], "system");
        assert_eq!(merged[2]["content"], "memory");
        assert_eq!(merged[3]["role"], "assistant");
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

    #[test]
    fn hoist_all_system_messages_moves_tail_system_to_a_single_leading_block() {
        // A self-hosted chat template (Qwen3) raises unless every system
        // message is first. The tail `memory` block that `merge_system_messages`
        // deliberately leaves in place must instead be hoisted and merged into
        // the leading system block for these dialects.
        let messages = vec![
            serde_json::json!({"role": "system", "content": "persona"}),
            serde_json::json!({"role": "system", "content": "tool hints"}),
            serde_json::json!({"role": "user", "content": "hi"}),
            serde_json::json!({"role": "system", "content": "memory"}),
            serde_json::json!({"role": "assistant", "content": "hello"}),
        ];
        let out = OpenAICompatibleProvider::hoist_all_system_messages(messages);
        // One leading system block carrying every system part, in order.
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["role"], "system");
        assert_eq!(out[0]["content"], "persona\n\ntool hints\n\nmemory");
        // No further system messages survive mid-stream.
        assert_eq!(out[1]["role"], "user");
        assert_eq!(out[2]["role"], "assistant");
        assert!(out[1..].iter().all(|m| m["role"] != "system"));
    }

    #[test]
    fn stringify_tool_call_arguments_turns_objects_into_json_strings() {
        // The exact shape the agent loop builds and persists: arguments is a
        // parsed JSON object. OpenAI/OpenRouter/Azure reject that with the
        // "expected a string, but got an object" 400.
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {
                    "name": "read_file",
                    "arguments": {"path": "a.txt", "lines": 10}
                }
            }]
        })];
        let out = OpenAICompatibleProvider::stringify_tool_call_arguments(messages);
        let args = out[0]["tool_calls"][0]["function"]["arguments"].clone();
        assert!(args.is_string(), "arguments must be a string, got {args}");
        assert_eq!(
            serde_json::from_str::<Value>(args.as_str().unwrap()).unwrap(),
            serde_json::json!({"path": "a.txt", "lines": 10})
        );
    }

    #[test]
    fn stringify_tool_call_arguments_leaves_strings_and_non_assistant_untouched() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "hi"}),
            serde_json::json!({
                "role": "assistant",
                "content": "ok",
                "tool_calls": [{
                    "id": "call_2",
                    "type": "function",
                    "function": {"name": "t", "arguments": "{\"x\": 1}"}
                }]
            }),
        ];
        let out = OpenAICompatibleProvider::stringify_tool_call_arguments(messages);
        // User message untouched (no tool_calls)...
        assert!(out[0].get("tool_calls").is_none());
        // ...and the already-string arguments are preserved verbatim.
        assert_eq!(
            out[1]["tool_calls"][0]["function"]["arguments"],
            serde_json::json!("{\"x\": 1}")
        );
    }

    #[test]
    fn stringify_tool_call_arguments_tolerates_missing_or_malformed_arguments() {
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "content": "ok",
            "tool_calls": [
                {"id": "a", "type": "function", "function": {"name": "t"}},
                {"id": "b", "type": "function", "function": {"name": "u", "arguments": ""}},
            ]
        })];
        let out = OpenAICompatibleProvider::stringify_tool_call_arguments(messages);
        // Missing arguments key is left absent; empty-string arguments stay a string.
        assert!(out[0]["tool_calls"][0]["function"].get("arguments").is_none());
        assert_eq!(
            out[0]["tool_calls"][1]["function"]["arguments"],
            serde_json::json!("")
        );
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
