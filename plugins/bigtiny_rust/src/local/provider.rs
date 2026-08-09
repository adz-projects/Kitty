//! `LocalProvider` — the in-process engine behind the normal [`Provider`]
//! trait (docs/ANDROID.md §3.1, §4.2).
//!
//! Implementing the same trait as the HTTP providers is what keeps the agent
//! loop, HITL, compaction and timings code entirely unaware that a turn was
//! served locally.
//!
//! Two things here are load-bearing and easy to get wrong:
//!
//! 1. **The chat template is mandatory.** Feeding a bare prompt to an
//!    instruct-tuned model makes it emit EOS immediately — zero tokens, which
//!    looks *identical* to a broken build. This cost a debugging cycle in
//!    Phase 1, so §9 records it and [`build_prompt`] enforces it. The template
//!    already emits BOS/turn markers, hence `AddBos::Never`.
//! 2. **Generation is blocking and CPU-bound.** It runs on `spawn_blocking`
//!    and streams out over a channel; it must never execute on the async
//!    runtime.
//!
//! **Tool calls are deliberately not supported.** The models this engine
//! targets (~1.2B) do not emit reliable tool calls, and a plausible-looking
//! wrong one is worse than none. `chat_completion` never yields
//! `Delta::tool_calls`; a caller wanting tools should route to a cloud
//! provider (which on Android is the only chat path anyway, per D18).

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;
use serde_json::Value;

use crate::config::LocalEngineConfig;
use crate::error::ProviderError;
use crate::provider::base::{Delta, HealthStatus, ModelInfo, Provider, SamplingParams};

use super::engine::LocalEngine;
use super::manager::{SlotKind, SlotManager};

/// Hard ceiling on one turn, so a degenerate model can't spin forever when
/// the caller supplied no `max_tokens`.
const DEFAULT_MAX_TOKENS: i32 = 2048;

pub struct LocalProvider {
    provider_id: String,
    slots: SlotManager,
    cfg: LocalEngineConfig,
    /// Reported model name. Cosmetic — the actual weights come from
    /// `cfg.model_path`; there is only ever one local model per slot.
    model_label: String,
}

impl LocalProvider {
    pub fn new(provider_id: &str, cfg: LocalEngineConfig, slots: SlotManager) -> Self {
        let model_label = std::path::Path::new(&cfg.model_path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "local".to_string());
        Self {
            provider_id: provider_id.to_string(),
            slots,
            cfg,
            model_label,
        }
    }
}

/// Render `messages` through the model's own chat template.
///
/// Falls back to a plain role-tagged transcript only when the GGUF carries no
/// template at all — better than sending raw text, which reliably produces an
/// instant EOS on instruct models.
fn build_prompt(engine: &LocalEngine, messages: &[Value]) -> Result<String, ProviderError> {
    use llama_cpp_2::model::LlamaChatMessage;

    let pairs: Vec<(String, String)> = messages
        .iter()
        .filter_map(|m| {
            let role = m.get("role")?.as_str()?.to_string();
            // Multimodal content arrives as an array of parts; keep the text
            // parts and drop the rest — this engine has no vision path.
            let content = match m.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(parts)) => parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            Some((role, content))
        })
        .collect();

    if pairs.is_empty() {
        return Err(ProviderError::Other {
            user_message: "no messages to send".into(),
            raw_message: "empty message list".into(),
            http_status: 400,
        });
    }

    match engine.model().chat_template(None) {
        Ok(tmpl) => {
            let chat: Result<Vec<LlamaChatMessage>, _> = pairs
                .iter()
                .map(|(r, c)| LlamaChatMessage::new(r.clone(), c.clone()))
                .collect();
            let chat = chat.map_err(|e| ProviderError::Other {
                user_message: "could not build the prompt".into(),
                raw_message: e.to_string(),
                http_status: 500,
            })?;
            engine
                .model()
                .apply_chat_template(&tmpl, &chat, true)
                .map_err(|e| ProviderError::Other {
                    user_message: "could not apply the model's chat template".into(),
                    raw_message: e.to_string(),
                    http_status: 500,
                })
        }
        Err(e) => {
            tracing::warn!("GGUF has no chat template ({e}); using a plain transcript");
            let mut s = String::new();
            for (role, content) in &pairs {
                s.push_str(role);
                s.push_str(": ");
                s.push_str(content);
                s.push('\n');
            }
            s.push_str("assistant: ");
            Ok(s)
        }
    }
}

/// Build the sampler chain from resolved [`SamplingParams`].
///
/// `temperature <= 0` means greedy — the deterministic path compaction wants.
/// Otherwise the order matters and mirrors llama.cpp's own recommended chain:
/// penalties → top_k → top_p → min_p → temp → dist.
fn build_sampler(sampling: &SamplingParams) -> llama_cpp_2::sampling::LlamaSampler {
    use llama_cpp_2::sampling::LlamaSampler;

    let temp = sampling.temperature.unwrap_or(0.8) as f32;
    if temp <= 0.0 {
        return LlamaSampler::chain_simple([LlamaSampler::greedy()]);
    }

    let mut stages = Vec::new();
    if sampling.presence_penalty.is_some() || sampling.frequency_penalty.is_some() {
        stages.push(LlamaSampler::penalties(
            64,
            1.0,
            sampling.frequency_penalty.unwrap_or(0.0) as f32,
            sampling.presence_penalty.unwrap_or(0.0) as f32,
        ));
    }
    if let Some(k) = sampling.top_k {
        if k > 0 {
            stages.push(LlamaSampler::top_k(k));
        }
    }
    if let Some(p) = sampling.top_p {
        stages.push(LlamaSampler::top_p(p as f32, 1));
    }
    if let Some(p) = sampling.min_p {
        stages.push(LlamaSampler::min_p(p as f32, 1));
    }
    stages.push(LlamaSampler::temp(temp));
    // llama.cpp's LLAMA_DEFAULT_SEED sentinel: seed randomly per call. A
    // fixed seed here would make every sampled turn in a session identical.
    stages.push(LlamaSampler::dist(u32::MAX));
    LlamaSampler::chain_simple(stages)
}

/// One blocking generation pass, streaming tokens out over `tx`.
///
/// Errors become a terminal `Delta` with `error_type` set rather than a
/// dropped stream, so the agent loop reports something actionable instead of
/// an empty response.
fn generate_blocking(
    engine: Arc<LocalEngine>,
    prompt: String,
    sampling: SamplingParams,
    tx: tokio::sync::mpsc::UnboundedSender<Delta>,
) {
    use llama_cpp_2::llama_batch::LlamaBatch;
    use llama_cpp_2::model::AddBos;

    let emit_err = |msg: String| {
        // Log as well as emit. `Delta::error_type` is **not read** by
        // `agent::loop_::process_stream` (a known dead field — see the 88bugs
        // re-audit, #62), so a failure signalled only that way is invisible:
        // the turn just produces nothing. Until that's wired up, the log is
        // the only place this surfaces.
        tracing::error!("local generation failed: {msg}");
        let _ = tx.send(Delta {
            role: "assistant".into(),
            // Surface it as content too, so the user sees *something* rather
            // than an empty reply.
            content: Some(format!("[local engine error: {msg}]")),
            reasoning: None,
            tool_calls: None,
            finish_reason: Some("error".into()),
            usage: None,
            error_type: Some(msg),
        });
    };

    let mut ctx = match engine.generation_context() {
        Ok(c) => c,
        Err(e) => return emit_err(e.to_string()),
    };
    let model = engine.model();

    // `AddBos::Never`: the chat template already emitted the BOS/turn markers.
    let tokens = match model.str_to_token(&prompt, AddBos::Never) {
        Ok(t) => t,
        Err(e) => return emit_err(format!("tokenize failed: {e}")),
    };
    if tokens.is_empty() {
        return emit_err("prompt tokenized to nothing".into());
    }

    let n_ctx = ctx.n_ctx() as i32;
    let max_new = sampling
        .max_tokens
        .filter(|m| *m > 0)
        .unwrap_or(DEFAULT_MAX_TOKENS)
        .min(n_ctx.saturating_sub(tokens.len() as i32).max(0));
    if max_new <= 0 {
        return emit_err(format!(
            "prompt ({} tokens) leaves no room in a {n_ctx}-token context",
            tokens.len()
        ));
    }

    // Prefill in `n_batch`-sized chunks. Submitting the whole prompt in one
    // batch works only while it fits: llama.cpp rejects (and on some builds
    // aborts on) a batch larger than the context's `n_batch`, and an agent
    // turn's prompt — system prompt plus history — is routinely several times
    // the 512-token default. Chunking is the normal way to prefill, not a
    // workaround.
    let prompt_tokens = tokens.len() as i32;
    let n_batch = (ctx.n_batch() as usize).max(1);
    let mut batch = LlamaBatch::new(n_batch.min(tokens.len()).max(1), 1);
    let last = tokens.len() - 1;
    tracing::debug!(
        prompt_tokens,
        n_batch,
        n_ctx,
        max_new,
        "local generation: prefill starting"
    );
    for (chunk_start, chunk) in tokens.chunks(n_batch).enumerate().map(|(i, c)| (i * n_batch, c)) {
        batch.clear();
        for (offset, token) in chunk.iter().copied().enumerate() {
            let pos = chunk_start + offset;
            // Logits are only needed for the very last prompt token — that's
            // the one the first sample reads.
            if let Err(e) = batch.add(token, pos as i32, &[0], pos == last) {
                return emit_err(format!("batch add failed: {e}"));
            }
        }
        if let Err(e) = ctx.decode(&mut batch) {
            return emit_err(format!("prompt decode failed: {e}"));
        }
    }
    tracing::debug!(prompt_tokens, "local generation: prefill done");

    let mut sampler = build_sampler(&sampling);
    let mut decoder = encoding_rs::UTF_8.new_decoder();
    // Absolute position of the next token — the whole prompt, not just the
    // final prefill chunk.
    let mut n_cur = prompt_tokens;
    let mut produced = 0i32;
    let mut finish = "stop";

    while produced < max_new {
        let token = sampler.sample(&ctx, batch.n_tokens() - 1);
        sampler.accept(token);
        if model.is_eog_token(token) {
            break;
        }
        match model.token_to_piece(token, &mut decoder, true, None) {
            Ok(piece) if !piece.is_empty() => {
                // A closed receiver means the caller went away (cancelled
                // turn, dropped stream) — stop burning CPU on it.
                if tx
                    .send(Delta {
                        role: "assistant".into(),
                        content: Some(piece),
                        reasoning: None,
                        tool_calls: None,
                        finish_reason: None,
                        usage: None,
                        error_type: None,
                    })
                    .is_err()
                {
                    return;
                }
            }
            Ok(_) => {}
            Err(e) => return emit_err(format!("detokenize failed: {e}")),
        }

        batch.clear();
        if let Err(e) = batch.add(token, n_cur, &[0], true) {
            return emit_err(format!("batch add failed: {e}"));
        }
        n_cur += 1;
        produced += 1;
        if let Err(e) = ctx.decode(&mut batch) {
            return emit_err(format!("decode failed: {e}"));
        }
    }
    if produced >= max_new {
        finish = "length";
    }
    tracing::debug!(produced, finish, "local generation: done");

    let mut usage = std::collections::HashMap::new();
    usage.insert("prompt_tokens".to_string(), prompt_tokens);
    usage.insert("completion_tokens".to_string(), produced);
    usage.insert("total_tokens".to_string(), prompt_tokens + produced);
    let _ = tx.send(Delta {
        role: "assistant".into(),
        content: None,
        reasoning: None,
        tool_calls: None,
        finish_reason: Some(finish.into()),
        usage: Some(usage),
        error_type: None,
    });
}

#[async_trait]
impl Provider for LocalProvider {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn resolve_model(&self, override_model: Option<&str>) -> String {
        // There is only one model per slot, so an override is informational.
        override_model
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.model_label)
            .to_string()
    }

    async fn chat_completion(
        &self,
        messages: Vec<Value>,
        tools: Option<Vec<Value>>,
        sampling: SamplingParams,
        _model: Option<String>,
        _id_slot: Option<i32>,
    ) -> Result<Pin<Box<dyn Stream<Item = Delta> + Send>>, ProviderError> {
        if tools.as_ref().is_some_and(|t| !t.is_empty()) {
            // Loud, not silent: a caller that needs tools must know this
            // provider can't serve them rather than getting a toolless reply
            // and wondering why the agent never acts.
            tracing::warn!(
                provider = %self.provider_id,
                "local provider does not support tool calls; they will be ignored"
            );
        }

        let slots = self.slots.clone();
        let cfg = self.cfg.clone();
        let engine = tokio::task::spawn_blocking(move || slots.get_or_load(SlotKind::Summarizer, &cfg))
            .await
            .map_err(|e| ProviderError::Other {
                user_message: "local engine task failed".into(),
                raw_message: e.to_string(),
                http_status: 500,
            })?
            .map_err(|e| ProviderError::Other {
                user_message: format!("local model unavailable: {e}"),
                raw_message: e.to_string(),
                http_status: 503,
            })?;

        let prompt = build_prompt(&engine, &messages)?;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::task::spawn_blocking(move || generate_blocking(engine, prompt, sampling, tx));
        Ok(Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(
            rx,
        )))
    }

    async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        if self.cfg.model_path.trim().is_empty() {
            return Ok(vec![]);
        }
        Ok(vec![ModelInfo {
            id: self.model_label.clone(),
            name: Some(self.model_label.clone()),
            provider_id: Some(self.provider_id.clone()),
            context_length: Some(self.cfg.n_ctx as i32),
        }])
    }

    async fn check_health(&self) -> HealthStatus {
        // Report on the *configured* model without forcing a load: health is
        // polled, and a poll that pulls hundreds of MB off disk would be a
        // denial of service against ourselves.
        if !self.cfg.enabled {
            return HealthStatus {
                status: "disconnected".into(),
                latency_ms: None,
                error: Some("local engine disabled".into()),
            };
        }
        let path = self.cfg.model_path.trim();
        if path.is_empty() {
            return HealthStatus {
                status: "disconnected".into(),
                latency_ms: None,
                error: Some("no local model configured".into()),
            };
        }
        if self.slots.get(SlotKind::Summarizer).is_some() {
            return HealthStatus {
                status: "connected".into(),
                latency_ms: Some(0.0),
                error: None,
            };
        }
        if std::path::Path::new(path).is_file() {
            HealthStatus {
                status: "connected".into(),
                latency_ms: None,
                error: None,
            }
        } else {
            HealthStatus {
                status: "disconnected".into(),
                latency_ms: None,
                error: Some(format!("model file not found: {path}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(path: &str) -> LocalEngineConfig {
        LocalEngineConfig {
            enabled: true,
            model_path: path.into(),
            ..Default::default()
        }
    }

    #[test]
    fn model_label_comes_from_the_gguf_filename() {
        let p = LocalProvider::new("local", cfg("/models/LFM2.5-1.2B-Q4.gguf"), SlotManager::new());
        assert_eq!(p.resolve_model(None), "LFM2.5-1.2B-Q4");
    }

    /// Sampler construction must survive every combination the config can
    /// produce — including the degenerate ones (`top_k: 0`, negative temp,
    /// all-None) that a hand-edited config or an unusual provider row can
    /// deliver. A panic here would take down a turn, not just misconfigure it.
    #[test]
    fn sampler_construction_tolerates_degenerate_params() {
        let cases = [
            SamplingParams::default(),
            SamplingParams {
                temperature: Some(0.0),
                ..Default::default()
            },
            SamplingParams {
                temperature: Some(-1.0),
                ..Default::default()
            },
            SamplingParams {
                temperature: Some(0.7),
                top_k: Some(0),
                top_p: Some(1.0),
                min_p: Some(0.0),
                presence_penalty: Some(0.0),
                frequency_penalty: Some(0.0),
                ..Default::default()
            },
        ];
        for c in cases {
            let _ = build_sampler(&c);
        }
    }

    #[tokio::test]
    async fn health_is_specific_about_why_it_is_down() {
        let disabled = LocalProvider::new(
            "local",
            LocalEngineConfig::default(),
            SlotManager::new(),
        );
        let h = disabled.check_health().await;
        assert_eq!(h.status, "disconnected");
        assert!(h.error.unwrap().contains("disabled"));

        let missing = LocalProvider::new("local", cfg("no-such-model.gguf"), SlotManager::new());
        let h = missing.check_health().await;
        assert_eq!(h.status, "disconnected");
        assert!(
            h.error.unwrap().contains("not found"),
            "a missing file and a disabled engine need different messages"
        );
    }

    /// Health polling must never trigger a model load — it runs every few
    /// seconds and the model is hundreds of MB.
    #[tokio::test]
    async fn health_check_does_not_load_the_model() {
        let slots = SlotManager::new();
        let p = LocalProvider::new("local", cfg("no-such-model.gguf"), slots.clone());
        let _ = p.check_health().await;
        assert!(slots.get(SlotKind::Summarizer).is_none());
    }

    /// End-to-end through the real trait with a real GGUF, opt-in via
    /// `KITTY_TEST_CHAT_GGUF`. This is the only test that proves the whole
    /// path — chat template, tokenize, sample, stream, terminal usage delta —
    /// actually produces text.
    ///
    /// It specifically guards the Phase 1 failure mode: without the chat
    /// template an instruct model emits EOS immediately and yields **zero**
    /// content deltas, which is indistinguishable from a broken build unless
    /// something asserts on it.
    #[tokio::test]
    async fn end_to_end_generation_produces_text_and_a_usage_delta() {
        use futures::StreamExt;

        let Ok(path) = std::env::var("KITTY_TEST_CHAT_GGUF") else {
            eprintln!("skipping: set KITTY_TEST_CHAT_GGUF to a GGUF path to run");
            return;
        };
        let p = LocalProvider::new("local", cfg(&path), SlotManager::new());
        let stream = p
            .chat_completion(
                vec![json!({"role":"user","content":"Reply with exactly: ok"})],
                None,
                SamplingParams {
                    temperature: Some(0.0),
                    max_tokens: Some(16),
                    ..Default::default()
                },
                None,
                None,
            )
            .await
            .expect("chat_completion should succeed with a real model");

        let deltas: Vec<Delta> = stream.collect().await;
        let text: String = deltas.iter().filter_map(|d| d.content.clone()).collect();
        let errors: Vec<&String> = deltas.iter().filter_map(|d| d.error_type.as_ref()).collect();

        assert!(errors.is_empty(), "unexpected error deltas: {errors:?}");
        assert!(
            !text.trim().is_empty(),
            "no content produced — the classic symptom of a missing chat template"
        );
        let last = deltas.last().expect("at least one delta");
        assert!(
            last.finish_reason.is_some(),
            "the stream must end with a finish_reason"
        );
        let usage = last.usage.as_ref().expect("terminal delta carries usage");
        assert!(usage.get("completion_tokens").copied().unwrap_or(0) > 0);
        assert!(usage.get("prompt_tokens").copied().unwrap_or(0) > 0);
    }

    #[tokio::test]
    async fn discover_models_is_empty_when_unconfigured() {
        let p = LocalProvider::new("local", LocalEngineConfig::default(), SlotManager::new());
        assert!(p.discover_models().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn chat_completion_reports_an_unavailable_model_rather_than_hanging() {
        let p = LocalProvider::new("local", cfg("no-such-model.gguf"), SlotManager::new());
        // `Pin<Box<dyn Stream>>` isn't Debug, so match rather than unwrap_err.
        let msg = match p
            .chat_completion(
                vec![json!({"role":"user","content":"hi"})],
                None,
                SamplingParams::default(),
                None,
                None,
            )
            .await
        {
            Ok(_) => panic!("expected an error for a missing model file"),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.to_lowercase().contains("unavailable") || msg.contains("not found"),
            "got {msg}"
        );
    }
}
