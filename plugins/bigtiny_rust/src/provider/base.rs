use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::pin::Pin;
use std::time::Duration;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effort {
    /// Explicitly ask the model *not* to spend reasoning tokens. Expressible
    /// on OpenRouter (`{"enabled": false}`) and by omission on Anthropic;
    /// OpenAI's o-series has no "off", so this degrades to "send nothing".
    Off,
    Low,
    Medium,
    High,
    /// A model-declared level that isn't one of the standard three — e.g.
    /// Qwen3's `xhigh`, discovered from a self-hosted server's chat template
    /// (see Kitty's `bigtiny::effort`). Forwarded verbatim to the self-hosted
    /// wire path; the hosted dialects (OpenAI/OpenRouter/Anthropic), which only
    /// understand low/medium/high, clamp it to their highest level.
    Custom(String),
}

impl Effort {
    /// Parse the wire string Kitty persists in session config
    /// (`thinking_effort`). Empty → `None` ("no effort requested"); `off` →
    /// `Off`; the three standard levels map to their variants; anything else is
    /// a model-specific level carried through as `Custom` (Kitty only ever sends
    /// a level it actually discovered for the active model).
    pub fn from_wire(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" => None,
            "off" => Some(Effort::Off),
            "low" => Some(Effort::Low),
            "medium" => Some(Effort::Medium),
            "high" => Some(Effort::High),
            other => Some(Effort::Custom(other.to_string())),
        }
    }

    /// The raw level string for the self-hosted wire path (the chat template's
    /// `reasoning_effort` kwarg), or `None` for `Off`.
    pub fn wire_level(&self) -> Option<&str> {
        match self {
            Effort::Off => None,
            Effort::Low => Some("low"),
            Effort::Medium => Some("medium"),
            Effort::High => Some("high"),
            Effort::Custom(s) => Some(s.as_str()),
        }
    }

    /// Clamp to one of the three levels the hosted OpenAI/OpenRouter dialects
    /// accept. `None` for `Off`; a `Custom` level (e.g. `xhigh`) maps to `high`.
    pub fn hosted_level(&self) -> Option<&'static str> {
        match self {
            Effort::Off => None,
            Effort::Low => Some("low"),
            Effort::Medium => Some("medium"),
            Effort::High | Effort::Custom(_) => Some("high"),
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

/// Read the error body of a non-2xx response, bounded in both time and size.
/// A stalled body after error headers (captive portal / proxy returning
/// `500`/`429` then going silent) used to hang the turn forever —
/// `resp.text().await` has no bound and the shared client only sets
/// `connect_timeout`. Only the first ~300 chars are ever surfaced by
/// `classify_provider_error`, so stopping after a few KB loses nothing real.
pub async fn read_bounded_error_body(resp: reqwest::Response) -> String {
    const ERROR_BODY_READ_TIMEOUT: Duration = Duration::from_secs(5);
    const ERROR_BODY_MAX_BYTES: usize = 8 * 1024;
    let mut resp = resp;
    let mut out: Vec<u8> = Vec::new();
    let read = async {
        while out.len() < ERROR_BODY_MAX_BYTES {
            match resp.chunk().await {
                Ok(Some(c)) => {
                    let need = ERROR_BODY_MAX_BYTES - out.len();
                    out.extend_from_slice(&c[..c.len().min(need)]);
                }
                _ => break,
            }
        }
    };
    // A timeout here is fine — the partial body read so far is strictly better
    // than a turn hanging on an unresponsive error body.
    tokio::time::timeout(ERROR_BODY_READ_TIMEOUT, read).await.ok();
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse a `Retry-After` header value (seconds form — the overwhelmingly
/// common case; HTTP-date values are returned as `None` rather than guessed
/// at). Providers return it on 429/503 to say how long to back off before
/// retrying; honoring it keeps the daemon from hammering a rate-limited
/// endpoint with our own (shorter) backoff.
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Classify an HTTP error into a structured provider error. `retry_after`
/// is the parsed `Retry-After` hint from the response headers (if any) —
/// carried on the resulting `Other` variant so the retry loop can honor it.
pub fn classify_provider_error(
    status_code: u16,
    body: &str,
    retry_after: Option<u64>,
) -> ProviderError {
    let lower = body.to_lowercase();

    // Checked by status code, not keyword-matched — 401/403 is unambiguous
    // where it's available (release-fixes item 27; `status_code` is 0 for
    // the handful of call sites with no real HTTP response to classify,
    // e.g. `discover_models`, so this never misfires there).
    if status_code == 401 || status_code == 403 {
        return ProviderError::AuthFailed {
            user_message: "Authentication failed. Check the API key for this provider.".into(),
            raw_message: body.into(),
            http_status: status_code as i32,
        };
    }

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

    // Anthropic says "input length and max_tokens exceed context limit" — it
    // says *limit*, not *maximum*, so it matched none of the three original
    // arms and fell through to the retryable `Other` below. The consequences
    // were compounding: the attempt loop retried an identical, guaranteed-fatal
    // request with backoff, failed over to a second provider and failed there
    // too, and finally surfaced an untagged error, so `wire_type_tag` was
    // `None` and none of Kitty's `context_exceeded` handling (the "New Session"
    // affordance, the session-concluded state) ever engaged. It is also the
    // exact wording the wrap-up valve provokes when its `max_tokens` clamp is
    // wrong, which is why this is the arm that must be right.
    if lower.contains("context_length_exceeded")
        || lower.contains("context") && lower.contains("maximum")
        || lower.contains("context") && lower.contains("limit")
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
        retry_after_secs: retry_after,
    }
}

/// Map a failed `reqwest::Error` to the most specific transport-class
/// `ProviderError` variant (see #11). `reqwest` reports connect failures,
/// timeouts, and mid-flight request failures all through the same error
/// type; before this they were lumped into a single catch-all `Request`, so
/// the retry policy and wire tags could not tell "the network path is down"
/// apart from "the peer is stalling". `http_status` is always `0` — a
/// transport failure never produced an HTTP response.
pub fn classify_transport_error(e: &reqwest::Error, user_message: String) -> ProviderError {
    let raw_message = e.to_string();
    if e.is_timeout() {
        ProviderError::Timeout {
            user_message,
            raw_message,
            http_status: 0,
        }
    } else if e.is_connect() {
        ProviderError::ConnectFailed {
            user_message,
            raw_message,
            http_status: 0,
        }
    } else {
        ProviderError::Request {
            user_message,
            raw_message,
            http_status: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_insufficient_credits() {
        let err = classify_provider_error(402, "Billing issue", None);
        match err {
            ProviderError::InsufficientCredits { .. } => {}
            other => panic!("Expected InsufficientCredits, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_context_exceeded() {
        let err = classify_provider_error(400, "context_length_exceeded", None);
        match err {
            ProviderError::ContextExceeded { .. } => {}
            other => panic!("Expected ContextExceeded, got {:?}", other),
        }
    }

    /// The real Anthropic 400 body, which used to be classified as a
    /// *retryable* `Other`: it says "context limit", and the only arms that
    /// existed looked for "context_length_exceeded", "context"+"maximum", or
    /// "too long". A guaranteed-fatal request was therefore retried with
    /// backoff, failed over to a second provider, and surfaced untagged.
    #[test]
    fn anthropic_input_plus_max_tokens_wording_is_context_exceeded() {
        for body in [
            "input length and `max_tokens` exceed context limit: 199000 + 20480 > 200000",
            "prompt is too long: 210000 tokens > 200000 maximum",
            "This model's maximum context length is 128000 tokens",
            "context_length_exceeded",
        ] {
            let err = classify_provider_error(400, body, None);
            assert!(
                matches!(err, ProviderError::ContextExceeded { .. }),
                "{body:?} must classify as ContextExceeded, got {err:?}"
            );
            // Non-retryable is the half that stops the retry storm, and the
            // wire tag is the half that lets Kitty offer "New Session".
            let err = classify_provider_error(400, body, None);
            assert!(!err.is_retryable(), "{body:?} must not be retried");
            assert_eq!(err.wire_type_tag(), Some("context_exceeded"), "{body:?}");
        }
    }

    /// Negative control for the arm above: "limit" and "context" have to
    /// co-occur, so an unrelated limit error stays a plain retryable `Other`
    /// rather than being swept up as a context overflow.
    #[test]
    fn an_unrelated_limit_error_is_not_context_exceeded() {
        for body in ["rate limit exceeded", "request limit reached", "concurrency limit"] {
            let err = classify_provider_error(429, body, None);
            assert!(
                !matches!(err, ProviderError::ContextExceeded { .. }),
                "{body:?} must not be mistaken for a context overflow"
            );
        }
    }

    #[test]
    fn test_classify_other() {
        let err = classify_provider_error(500, "Internal error", None);
        match err {
            ProviderError::Other { .. } => {}
            other => panic!("Expected Other, got {:?}", other),
        }
    }

    /// #2 regression: a non-2xx response whose body stalls (captive portal /
    /// broken proxy returns headers then goes silent) used to hang the turn
    /// forever via `resp.text().await`. The bounded read must return the
    /// partial body quickly instead of waiting out the stall.
    #[tokio::test]
    async fn read_bounded_error_body_returns_quickly_on_a_stalled_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await;
            // 500 headers, then a tiny partial body, then silence for 30s.
            let head = "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 1000\r\nconnection: close\r\n\r\n";
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(b"partial error bo").await.unwrap();
            sock.flush().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap();

        let start = std::time::Instant::now();
        let body = read_bounded_error_body(resp).await;
        // The 5s cap, not the 30s stall, must bound the read.
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "bounded error-body read must not wait out the 30s stall"
        );
        assert!(body.starts_with("partial error bo"));
    }

    #[tokio::test]
    async fn read_bounded_error_body_caps_oversized_bodies() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let head = "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 1048576\r\nconnection: close\r\n\r\n";
            sock.write_all(head.as_bytes()).await.unwrap();
            let big = vec![b'x'; 1024 * 1024];
            sock.write_all(&big).await.unwrap();
            sock.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap();

        let body = read_bounded_error_body(resp).await;
        assert!(
            body.len() <= 8 * 1024,
            "error body must be capped to a few KB, got {} bytes",
            body.len()
        );
    }

    #[test]
    fn test_classify_auth_failed_on_401_and_403() {
        for status in [401u16, 403] {
            let err = classify_provider_error(status, "unauthorized", None);
            match err {
                ProviderError::AuthFailed { http_status, .. } => {
                    assert_eq!(http_status, status as i32)
                }
                other => panic!("Expected AuthFailed for {status}, got {:?}", other),
            }
        }
    }

    #[test]
    fn parse_retry_after_parses_seconds_and_ignores_garbage() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "30".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(30));

        assert_eq!(parse_retry_after(&reqwest::header::HeaderMap::new()), None);

        // HTTP-date form is returned as `None` rather than guessed at.
        let mut date = reqwest::header::HeaderMap::new();
        date.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2026 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(parse_retry_after(&date), None);
    }

    #[test]
    fn retryability_matches_the_fatal_classifications() {
        assert!(!classify_provider_error(401, "unauthorized", None).is_retryable());
        assert!(!classify_provider_error(403, "forbidden", None).is_retryable());
        assert!(!classify_provider_error(402, "billing issue", None).is_retryable());
        assert!(!classify_provider_error(400, "context_length_exceeded", None).is_retryable());

        assert!(classify_provider_error(500, "boom", None).is_retryable());
        assert!(classify_provider_error(429, "rate limited", None).is_retryable());
        assert!(classify_provider_error(503, "unavailable", None).is_retryable());
        assert!(
            ProviderError::Request {
                user_message: "x".into(),
                raw_message: "x".into(),
                http_status: 0,
            }
            .is_retryable()
        );
    }

    #[test]
    fn classify_carries_the_retry_after_hint_on_rate_limited_errors() {
        let err = classify_provider_error(429, "rate limited", Some(17));
        match err {
            ProviderError::Other {
                http_status,
                retry_after_secs,
                ..
            } => {
                assert_eq!(http_status, 429);
                assert_eq!(retry_after_secs, Some(17));
            }
            other => panic!("Expected Other, got {:?}", other),
        }
        // A fatal classification drops the hint — it's not going to be retried.
        let fatal = classify_provider_error(401, "unauthorized", Some(5));
        assert_eq!(fatal.retry_after(), None);
        assert!(!fatal.is_retryable());
    }

    #[test]
    fn wire_type_tag_covers_the_frontend_actionable_variants() {
        assert_eq!(
            classify_provider_error(402, "credit", None).wire_type_tag(),
            Some("insufficient_credits")
        );
        assert_eq!(
            classify_provider_error(400, "context_length_exceeded", None).wire_type_tag(),
            Some("context_exceeded")
        );
        assert_eq!(
            classify_provider_error(401, "unauthorized", None).wire_type_tag(),
            Some("auth_failed")
        );
        assert_eq!(classify_provider_error(500, "boom", None).wire_type_tag(), None);
        assert_eq!(
            ProviderError::Request {
                user_message: "x".into(),
                raw_message: "x".into(),
                http_status: 0,
            }
            .wire_type_tag(),
            Some("network_unreachable")
        );
    }

    /// A huge provider error body must not land verbatim in the user-facing
    /// message (it can echo request content back); the full text stays in
    /// `raw_message` for diagnostics.
    #[test]
    fn test_classify_other_truncates_the_user_facing_body() {
        let huge = "x".repeat(10_000);
        let err = classify_provider_error(500, &huge, None);
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

    /// #11: the transport-class variants (`Request`/`ConnectFailed`/
    /// `Timeout`) all mean "can't reach the provider" — they must keep the
    /// `network_unreachable` wire tag, stay retryable, and flag as transport
    /// errors so the passive circuit breaker catches them all.
    #[test]
    fn transport_class_variants_tag_network_unreachable_and_stay_retryable() {
        for e in [
            ProviderError::Request {
                user_message: "x".into(),
                raw_message: "x".into(),
                http_status: 0,
            },
            ProviderError::ConnectFailed {
                user_message: "x".into(),
                raw_message: "x".into(),
                http_status: 0,
            },
            ProviderError::Timeout {
                user_message: "x".into(),
                raw_message: "x".into(),
                http_status: 0,
            },
        ] {
            assert_eq!(e.wire_type_tag(), Some("network_unreachable"), "{e:?}");
            assert!(e.is_retryable(), "{e:?}");
            assert!(e.is_transport_error(), "{e:?}");
        }
        // An HTTP-classified failure (a real response arrived) is none of
        // those.
        assert!(!classify_provider_error(500, "boom", None).is_transport_error());
    }

    /// #11: a refused local port (immediate TCP RST) must classify as
    /// `ConnectFailed`, not a generic `Request`.
    #[tokio::test]
    async fn a_refused_local_port_classifies_as_connect_failed() {
        let client = reqwest::Client::builder().build().unwrap();
        let err = client
            .get("http://127.0.0.1:1/")
            .send()
            .await
            .expect_err("port 1 on loopback must be refused");
        assert!(err.is_connect(), "expected a connect-level error, got {err}");
        match classify_transport_error(&err, "test".into()) {
            ProviderError::ConnectFailed { .. } => {}
            other => panic!("expected ConnectFailed, got {other:?}"),
        }
    }

    /// #11: a peer that accepts the TCP connection but then goes silent must
    /// classify as `Timeout` (the network is up; the peer stalled), not
    /// `ConnectFailed`.
    #[tokio::test]
    async fn a_stalled_local_peer_classifies_as_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept, read the request, then hold the connection open silently.
        let hold = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_millis(300))
            .build()
            .unwrap();
        let err = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect_err("the stalled peer must time out");
        hold.abort();
        assert!(err.is_timeout(), "expected a timeout-class error, got {err}");
        match classify_transport_error(&err, "test".into()) {
            ProviderError::Timeout { .. } => {}
            other => panic!("expected Timeout, got {other:?}"),
        }
    }
}
