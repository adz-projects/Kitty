//! Structured-output summarizer chain (docs/ANDROID.md §4.3, D12).
//!
//! Successor to the old `agent::summarizer::SummarizerClient`, which spoke
//! Ollama's *native* `/api/chat` protocol directly — a hardcoded dependency
//! on Ollama that had no place surviving Phase 2b's retirement of managed
//! Ollama, even as a "fallback." Deleted rather than kept dark: a class whose
//! only job is talking to a process this app no longer runs is exactly the
//! kind of latent tie that phase was for.
//!
//! §4.3's chain, as actually implemented:
//!
//! 1. **Local summarizer** (`litert::LiteRtSummarizer`, Windows-only, feature
//!    `litert-engine`) — in-process LiteRT-LM generative model, grammar-free
//!    JSON extraction. Tried first when configured and enabled. (Android has no
//!    local summarizer at all — it goes straight to leg 2.)
//! 2. **`fallback = "session_model"`** (the default) — routes through the
//!    *same* [`ProviderRouter`] every chat turn uses, so whatever the user has
//!    configured (local, self-hosted, or a cloud key) serves the fallback too.
//!    No provider-specific code here: a plain "respond with JSON matching
//!    this schema" instruction appended to the messages, then
//!    [`crate::agent::json_extract::extract_json`] pulled out of the reply
//!    the same way the local path does.
//! 3. **`fallback = "off"`**, or every leg failed — an explicit `Err`.
//!    `agent::compaction::run_compaction` treats that as "skip this round,"
//!    never a failed turn.
//!
//! Two call shapes, because the two callers see different context:
//! - [`SummarizerChain::structured_chat_for_session`] — chat compaction. Has
//!   a session, so the fallback leg uses that session's own pinned provider.
//! - [`SummarizerChain::structured_chat`] (the plain [`StructuredChat`] impl)
//!   — adaptive-pathway's learn passes, which run outside any one session's
//!   context. The fallback leg uses the router's current default provider
//!   ([`ProviderRouter::resolve_provider`] with no preference).

use std::sync::Arc;

use serde_json::{json, Value};

use adaptive_pathway::traits::StructuredChat;

use crate::config::SummarizerConfig;
use crate::provider::base::SamplingParams;
use crate::provider::router::ProviderRouter;
use crate::provider::sampling;

use super::json_extract::extract_json;

/// Hard ceiling on the total text `collect_text` accumulates for one
/// summarization (see #13). A legitimate summarization is a few KB at most —
/// the fallback call requests at most 1024 output tokens. A runaway provider
/// (or an unconstrained repetition loop) used to grow this unboundedly,
/// pinning the compaction/learn task with ever-growing memory. Past the
/// ceiling, `collect_text` stops draining and fails — the caller treats that
/// as "skip this round," never as a failed turn.
const MAX_SUMMARIZER_TEXT_CHARS: usize = 200_000;

/// Overall cap for one `via_router` fallback call, from request to full
/// stream drain (see #13). The per-chunk SSE idle timeout bounds silent
/// gaps and the providers' total-stream-duration cap (#10) bounds
/// dribble-alive streams — but both can legally hold an hour of wall time,
/// and a compaction/learn task must not be pinned for that long. A
/// summarization that hasn't finished in five minutes isn't going to.
const SUMMARIZER_OVERALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

pub struct SummarizerChain {
    /// In-process local summarizer, when one is configured — the LiteRT-LM
    /// engine (`litert::LiteRtSummarizer`, Windows only). Implements
    /// [`StructuredChat`], so the chain is engine-agnostic. `None` = go straight
    /// to the router fallback (always the case on Android).
    local: Option<Arc<dyn StructuredChat + Send + Sync>>,
    router: Arc<ProviderRouter>,
    cfg: SummarizerConfig,
}

impl SummarizerChain {
    pub fn new(
        local: Option<Arc<dyn StructuredChat + Send + Sync>>,
        router: Arc<ProviderRouter>,
        cfg: SummarizerConfig,
    ) -> Self {
        Self { local, router, cfg }
    }

    /// Try the local engine, if one is configured. `None` means "not available
    /// or it failed" — either way the caller moves on to the next leg. An
    /// unavailable/unconfigured engine returns `Err` from `structured_chat`,
    /// which is handled here identically to a genuine failure.
    async fn try_local(&self, messages: &[Value], schema: &Value) -> Option<Value> {
        let local = self.local.as_ref()?;
        match local.structured_chat(messages.to_vec(), schema).await {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::debug!("local summarizer unavailable/failed, falling back: {e}");
                None
            }
        }
    }

    /// Ask `provider_id` (optionally with a specific `model`) to answer in
    /// JSON, and pull the object out of whatever text comes back.
    ///
    /// No provider gets special-cased: this is the same "instruct, then
    /// parse" approach every non-constrained-decode caller needs, whether the
    /// provider is a hosted API or a self-hosted server pointed at by a
    /// remote `ollama`-dialect profile (see `provider/sampling.rs` — that
    /// profile still gets its dedicated floor here too, via `router.sampling`).
    async fn via_router(
        &self,
        provider_id: &str,
        model: Option<String>,
        messages: Vec<Value>,
        schema: &Value,
    ) -> Result<Value, String> {
        let mut prompted = messages;
        prompted.push(json!({
            "role": "user",
            "content": format!(
                "Respond with JSON only, matching this schema:\n{}",
                serde_json::to_string(schema).unwrap_or_else(|_| "{}".into())
            ),
        }));

        // Deterministic-ish and short, same rationale as the local path's
        // near-greedy default — this is structured extraction, not prose.
        let requested = SamplingParams {
            temperature: Some(self.cfg.temperature),
            max_tokens: Some(1024),
            ..Default::default()
        };
        let sampling = sampling::merge(&requested, &self.router.sampling(provider_id));

        // Wrap the whole request + stream drain in an overall timeout (see
        // #13, SUMMARIZER_OVERALL_TIMEOUT): the per-chunk SSE idle timeout
        // only bounds gaps between bytes, and the provider's
        // total-stream-duration cap is long — neither may pin the compaction
        // or learn task for the full window, so this is a hard wall-clock cap.
        let outcome = tokio::time::timeout(SUMMARIZER_OVERALL_TIMEOUT, async {
            let stream = self
                .router
                .chat_completion(provider_id, prompted, None, sampling, model, None)
                .await
                .map_err(|e| format!("summarizer fallback call to '{provider_id}' failed: {e}"))?;
            collect_text(stream).await
        })
        .await;

        let text = match outcome {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(format!(
                    "summarizer fallback call to '{provider_id}' exceeded the {}s overall limit",
                    SUMMARIZER_OVERALL_TIMEOUT.as_secs()
                ))
            }
        };
        extract_json(&text)
            .ok_or_else(|| format!("provider '{provider_id}' produced no parseable JSON"))
    }

    /// Chat compaction's entry point — the fallback leg uses `provider_id`
    /// (the session's own pinned provider), not the daemon's default.
    ///
    /// `provider_id` is optional because the local leg must run regardless of
    /// whether one resolved: a fresh daemon with no providers registered yet
    /// (or a session whose provider vanished) should still get a local
    /// summarization pass, and only fail once *both* legs have nothing to
    /// offer — not skip local because there's nothing to fall back to.
    pub async fn structured_chat_for_session(
        &self,
        provider_id: Option<&str>,
        model: Option<String>,
        messages: Vec<Value>,
        schema: &Value,
    ) -> Result<Value, String> {
        if let Some(v) = self.try_local(&messages, schema).await {
            return Ok(v);
        }
        if self.cfg.fallback == "off" {
            return Err("local summarizer unavailable and fallback is disabled".into());
        }
        let provider_id = provider_id
            .ok_or_else(|| "no provider available for the summarizer fallback".to_string())?;
        self.via_router(provider_id, model, messages, schema).await
    }
}

/// Drain a chat-completion stream into its text. Any content is success —
/// providers that only ever signal failure via `error_type` (the 88bugs #62
/// dead field, still not read by the agent loop) still surface here as content.
/// The accumulated text is capped at [`MAX_SUMMARIZER_TEXT_CHARS`] (see #13):
/// a runaway provider can't grow the turn's memory without bound.
async fn collect_text(
    mut stream: std::pin::Pin<Box<dyn futures::Stream<Item = crate::provider::base::Delta> + Send>>,
) -> Result<String, String> {
    use futures::StreamExt;

    let mut text = String::new();
    let mut chars = 0usize;
    let mut error: Option<String> = None;
    while let Some(delta) = stream.next().await {
        if let Some(c) = delta.content {
            chars += c.chars().count();
            if chars > MAX_SUMMARIZER_TEXT_CHARS {
                return Err(format!(
                    "summarizer output exceeded the {}-char ceiling; provider not converging",
                    MAX_SUMMARIZER_TEXT_CHARS
                ));
            }
            text.push_str(&c);
        }
        if let Some(e) = delta.error_type {
            error = Some(e);
        }
    }
    if text.trim().is_empty() {
        return Err(error.unwrap_or_else(|| "provider returned no content".into()));
    }
    Ok(text)
}

/// Sessionless entry point for adaptive-pathway's learn passes, which have no
/// per-session provider to fall back to — uses the router's current default.
#[async_trait::async_trait]
impl StructuredChat for SummarizerChain {
    async fn structured_chat(&self, messages: Vec<Value>, schema: &Value) -> Result<Value, String> {
        if let Some(v) = self.try_local(&messages, schema).await {
            return Ok(v);
        }
        if self.cfg.fallback == "off" {
            return Err("local summarizer unavailable and fallback is disabled".into());
        }
        let (provider_id, model) = self
            .router
            .resolve_provider(None)
            .await
            .map_err(|e| format!("no summarizer fallback provider available: {e}"))?;
        self.via_router(&provider_id, model, messages, schema).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderConfig;

    fn cfg(fallback: &str) -> SummarizerConfig {
        SummarizerConfig {
            fallback: fallback.into(),
            ..Default::default()
        }
    }

    fn chain(router: Arc<ProviderRouter>, fallback: &str) -> SummarizerChain {
        SummarizerChain::new(None, router, cfg(fallback))
    }

    /// With no local summarizer and `fallback: "off"`, both entry points must
    /// fail cleanly rather than panic or hang, and never touch the router.
    #[tokio::test]
    async fn fallback_off_fails_without_calling_any_provider() {
        let router = Arc::new(ProviderRouter::default());
        let chain = chain(router, "off");

        let err = chain
            .structured_chat_for_session(Some("nonexistent"), None, vec![], &json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("fallback is disabled"), "got {err}");

        let err = StructuredChat::structured_chat(&chain, vec![], &json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("fallback is disabled"), "got {err}");
    }

    /// `session_model` against an unknown provider id must fail with a
    /// specific, actionable message — not a generic "no provider" error that
    /// leaves no clue which of possibly several providers was meant.
    #[tokio::test]
    async fn session_model_reports_which_provider_failed() {
        let router = Arc::new(ProviderRouter::default());
        let chain = chain(router, "session_model");
        let err = chain
            .structured_chat_for_session(Some("does-not-exist"), None, vec![], &json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("does-not-exist"), "got {err}");
    }

    /// A session whose provider genuinely can't be resolved (rather than one
    /// that resolved to something unreachable) must produce a distinct,
    /// specific error — and must not have skipped the local attempt to get
    /// there (there's nothing to assert on the local side without the
    /// feature, but this pins the message so a future refactor can't
    /// silently swap in a generic router error instead).
    #[tokio::test]
    async fn a_session_with_no_resolvable_provider_reports_it_specifically() {
        let router = Arc::new(ProviderRouter::default());
        let chain = chain(router, "session_model");
        let err = chain
            .structured_chat_for_session(None, None, vec![], &json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("no provider available"), "got {err}");
    }

    /// The sessionless path must fail distinctly when there is no default
    /// provider to resolve at all (a fresh daemon with none registered) —
    /// this exercises `resolve_provider` rather than `chat_completion`.
    #[tokio::test]
    async fn sessionless_fallback_fails_when_no_default_provider_exists() {
        let router = Arc::new(ProviderRouter::default());
        let chain = chain(router, "session_model");
        let err = StructuredChat::structured_chat(&chain, vec![], &json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("no summarizer fallback provider"), "got {err}");
    }

    /// `via_router` must actually reach a registered provider — this is the
    /// regression guard for the whole point of the rewrite: no Ollama-native
    /// client anywhere in the path, just the same router every chat turn uses.
    #[tokio::test]
    async fn via_router_reaches_a_registered_provider() {
        let mut server = mockito::Server::new_async().await;
        // OpenAI-compatible providers stream SSE, not a single JSON body —
        // even the daemon's own "non-streaming" request still gets parsed as
        // an SSE response, so the mock must speak that shape.
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"ok\\\":true}\"}}]}\n\n\
                   data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                   data: [DONE]\n\n";
        let _m = server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(sse)
            .create_async()
            .await;

        let router = Arc::new(ProviderRouter::default());
        router.register_openai(
            "p1",
            ProviderConfig {
                provider_type: "custom_openai".into(),
                base_url: server.url(),
                ..Default::default()
            },
        );
        let chain = chain(router, "session_model");

        let v = chain
            .structured_chat_for_session(
                Some("p1"),
                None,
                vec![json!({"role":"user","content":"hi"})],
                &json!({}),
            )
            .await
            .expect("the mocked provider must be reachable and its JSON extracted");
        assert_eq!(v, json!({"ok": true}));
    }

    /// #13: a runaway summarizer stream (output far past the 1024-token
    /// request) must trip the char ceiling and fail early instead of
    /// accumulating without bound.
    #[tokio::test]
    async fn collect_text_caps_runaway_output_at_the_char_ceiling() {
        use crate::provider::base::Delta;
        let runaway = "x".repeat(MAX_SUMMARIZER_TEXT_CHARS + 1);
        let stream: std::pin::Pin<Box<dyn futures::Stream<Item = Delta> + Send>> = Box::pin(
            futures::stream::iter(vec![Delta {
                role: "assistant".into(),
                content: Some(runaway),
                reasoning: None,
                tool_calls: None,
                finish_reason: None,
                usage: None,
                error_type: None,
            }]),
        );
        let err = collect_text(stream).await.unwrap_err();
        assert!(err.contains("ceiling"), "got {err}");
    }

    /// #13: ordinary (below-ceiling) output passes through unchanged, and an
    /// empty stream still surfaces the provider's `error_type` (or the
    /// generic no-content message) as before.
    #[tokio::test]
    async fn collect_text_passes_normal_output_and_reports_empty_streams() {
        use crate::provider::base::Delta;
        fn delta(content: Option<&str>, error_type: Option<&str>) -> Delta {
            Delta {
                role: "assistant".into(),
                content: content.map(|s| s.to_string()),
                reasoning: None,
                tool_calls: None,
                finish_reason: None,
                usage: None,
                error_type: error_type.map(|s| s.to_string()),
            }
        }
        let stream: std::pin::Pin<Box<dyn futures::Stream<Item = Delta> + Send>> = Box::pin(
            futures::stream::iter(vec![delta(Some("{\"ok\":true}"), None)]),
        );
        assert_eq!(collect_text(stream).await.unwrap(), "{\"ok\":true}");

        let stream: std::pin::Pin<Box<dyn futures::Stream<Item = Delta> + Send>> = Box::pin(
            futures::stream::iter(vec![delta(None, Some("request"))]),
        );
        assert_eq!(collect_text(stream).await.unwrap_err(), "request");
    }
}
