//! Local structured-output summarizer (docs/ANDROID.md §3.1, §4.3, D12).
//!
//! Implements the same [`StructuredChat`] seam `agent::summarizer_chain`'s
//! provider-router fallback does, so compaction and adaptive-pathway's
//! learned extraction don't care which one actually served a given call —
//! see that module for how the two are chained (local first, always).
//!
//! **Why there is no GBNF grammar here, despite §4.3 calling for one.**
//!
//! Grammar-constrained decoding was implemented and then deliberately removed:
//! llama.cpp's grammar sampler calls `GGML_ABORT` — a **process abort**, not a
//! returnable error — when the model emits EOG before the grammar is
//! satisfied (`llama-grammar.cpp`, `GGML_ASSERT(!stacks.empty())`). A small
//! model doing that is not exotic, and the daemon hosts chat, MCP and the
//! scheduler, so a hard abort there takes all of it down. There is no way to
//! catch it from Rust.
//!
//! Trading an occasional non-JSON response for a crash-free daemon is the
//! right side of that trade, especially since the chain below already handles
//! bad output. Revisit with the `llguidance` feature (a separate constraint
//! implementation with different failure behaviour) rather than by
//! re-enabling GBNF.
//!
//! So: generate, extract the JSON (models wrap it in prose or fences), then
//! validate against the real schema with `jsonschema` — already a dependency,
//! and stricter than a grammar could be anyway.
//!
//! §4.3's chain becomes: decode → temperature-burn retry → low-temperature
//! retry → explicit error. Compaction treats a final error as "skipped this
//! round", never a failed turn.

use std::sync::Arc;

use serde_json::Value;

use crate::config::{LocalEngineConfig, SummarizerConfig};

use super::engine::LocalEngine;
use super::manager::{SlotKind, SlotManager};

/// Cap on one summarization. Slot extraction is short by construction; a
/// runaway here would stall compaction rather than fail it.
const MAX_TOKENS: i32 = 1024;

pub struct LocalSummarizer {
    slots: SlotManager,
    local: LocalEngineConfig,
    cfg: SummarizerConfig,
}

impl LocalSummarizer {
    pub fn new(local: LocalEngineConfig, cfg: SummarizerConfig, slots: SlotManager) -> Self {
        Self { slots, local, cfg }
    }

    /// Whether this summarizer can run at all — checked before the chain so
    /// the caller can fall through to the session model (D12) without paying
    /// for a load attempt.
    pub fn is_available(&self) -> bool {
        self.local.enabled && !self.local.model_path.trim().is_empty()
    }
}

/// Render messages + an explicit schema instruction into a prompt.
///
/// The schema goes in the prompt *as well as* being enforced afterwards: the
/// grammar only guarantees well-formed JSON, so the model still needs to be
/// told which keys to produce.
fn build_prompt(engine: &LocalEngine, messages: &[Value], schema: &Value) -> Result<String, String> {
    use llama_cpp_2::model::LlamaChatMessage;

    let mut chat: Vec<LlamaChatMessage> = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
        let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
        chat.push(
            LlamaChatMessage::new(role.to_string(), content.to_string())
                .map_err(|e| e.to_string())?,
        );
    }
    chat.push(
        LlamaChatMessage::new(
            "user".into(),
            format!(
                "Respond with JSON only, matching this schema:\n{}",
                serde_json::to_string(schema).unwrap_or_else(|_| "{}".into())
            ),
        )
        .map_err(|e| e.to_string())?,
    );

    let tmpl = engine
        .model()
        .chat_template(None)
        .map_err(|e| format!("model has no chat template: {e}"))?;
    engine
        .model()
        .apply_chat_template(&tmpl, &chat, true)
        .map_err(|e| e.to_string())
}

/// One blocking generation to a `String`.
fn generate_to_string(
    engine: &LocalEngine,
    prompt: &str,
    temperature: f32,
) -> Result<String, String> {
    use llama_cpp_2::llama_batch::LlamaBatch;
    use llama_cpp_2::model::AddBos;
    use llama_cpp_2::sampling::LlamaSampler;

    let mut ctx = engine.generation_context().map_err(|e| e.to_string())?;
    let model = engine.model();

    let tokens = model
        .str_to_token(prompt, AddBos::Never)
        .map_err(|e| format!("tokenize failed: {e}"))?;
    if tokens.is_empty() {
        return Err("prompt tokenized to nothing".into());
    }
    let room = ctx.n_ctx() as i32 - tokens.len() as i32;
    if room <= 0 {
        return Err(format!(
            "prompt ({} tokens) does not fit the {}-token context",
            tokens.len(),
            ctx.n_ctx()
        ));
    }
    let budget = MAX_TOKENS.min(room);

    let mut batch = LlamaBatch::new(tokens.len().max(512), 1);
    let last = tokens.len() as i32 - 1;
    for (i, t) in (0i32..).zip(tokens.iter().copied()) {
        batch
            .add(t, i, &[0], i == last)
            .map_err(|e| format!("batch add failed: {e}"))?;
    }
    ctx.decode(&mut batch)
        .map_err(|e| format!("prompt decode failed: {e}"))?;

    let mut stages = Vec::new();
    if temperature <= 0.0 {
        stages.push(LlamaSampler::greedy());
    } else {
        stages.push(LlamaSampler::temp(temperature));
        stages.push(LlamaSampler::dist(u32::MAX));
    }
    let mut sampler = LlamaSampler::chain_simple(stages);

    let mut decoder = encoding_rs::UTF_8.new_decoder();
    let mut out = String::new();
    // `n_cur` is the KV-cache position, which happens to advance in lockstep
    // with the step count — it starts *after* the prompt, hence the zip
    // rather than a bare range.
    for (n_cur, _step) in (batch.n_tokens()..).zip(0..budget) {
        let token = sampler.sample(&ctx, batch.n_tokens() - 1);
        sampler.accept(token);
        if model.is_eog_token(token) {
            break;
        }
        if let Ok(piece) = model.token_to_piece(token, &mut decoder, true, None) {
            out.push_str(&piece);
        }

        // Stop once a complete JSON document has been produced, rather than
        // running to the token budget and letting the model append prose
        // after it. Cheap: only attempted once the text looks closed.
        if out.trim_end().ends_with('}') && serde_json::from_str::<Value>(out.trim()).is_ok() {
            break;
        }

        batch.clear();
        batch
            .add(token, n_cur, &[0], true)
            .map_err(|e| format!("batch add failed: {e}"))?;
        ctx.decode(&mut batch)
            .map_err(|e| format!("decode failed: {e}"))?;
    }
    Ok(out)
}

// `extract_json` moved to `agent::json_extract` — the provider-router
// fallback in `agent::summarizer_chain` needs the identical scan and isn't
// gated behind this module's `local-engine` feature, so it can't import from
// here.
use crate::agent::json_extract::extract_json;

fn validates(value: &Value, schema: &Value) -> bool {
    // An unusable schema shouldn't reject a good answer — treat it as "no
    // constraint" rather than failing the whole compaction pass.
    match jsonschema::validator_for(schema) {
        Ok(v) => v.is_valid(value),
        Err(e) => {
            tracing::warn!("summarizer schema is not compilable ({e}); skipping validation");
            true
        }
    }
}

#[async_trait::async_trait]
impl adaptive_pathway::traits::StructuredChat for LocalSummarizer {
    async fn structured_chat(&self, messages: Vec<Value>, schema: &Value) -> Result<Value, String> {
        if !self.is_available() {
            return Err("local summarizer is not configured".into());
        }
        let slots = self.slots.clone();
        let local = self.local.clone();
        let engine: Arc<LocalEngine> =
            tokio::task::spawn_blocking(move || slots.get_or_load(SlotKind::Summarizer, &local))
                .await
                .map_err(|e| format!("local engine task failed: {e}"))?
                .map_err(|e| e.to_string())?;

        let schema = schema.clone();
        let base_temp = self.cfg.temperature as f32;

        tokio::task::spawn_blocking(move || {
            let prompt = build_prompt(&engine, &messages, &schema)?;

            // §4.3's chain. The configured temperature first (usually near-
            // greedy, for determinism), then a burn retry — a malformed or
            // schema-invalid answer is usually a local minimum rather than an
            // incapacity, so resampling often fixes it — then a strictly
            // greedy pass as the most predictable last try.
            let attempts: [f32; 3] = [base_temp, (base_temp + 0.3).max(0.3), 0.0];

            let mut last_err = String::new();
            for (i, temp) in attempts.into_iter().enumerate() {
                match generate_to_string(&engine, &prompt, temp) {
                    Ok(raw) => match extract_json(&raw) {
                        Some(v) if validates(&v, &schema) => return Ok(v),
                        Some(_) => {
                            last_err = "output did not match the schema".into();
                            tracing::debug!(attempt = i, "summarizer output failed validation");
                        }
                        None => {
                            last_err = "output was not JSON".into();
                            tracing::debug!(attempt = i, "summarizer output was not JSON");
                        }
                    },
                    Err(e) => {
                        last_err = e;
                        tracing::debug!(attempt = i, error = %last_err, "summarizer attempt failed");
                    }
                }
            }
            Err(format!(
                "local summarizer produced no schema-valid JSON after {} attempts: {last_err}",
                attempts.len()
            ))
        })
        .await
        .map_err(|e| format!("summarizer task failed: {e}"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // `extract_json`'s own tests now live with the function, in
    // `agent::json_extract` — not duplicated here.

    #[test]
    fn schema_validation_accepts_and_rejects() {
        let schema = json!({
            "type": "object",
            "properties": { "n": { "type": "integer" } },
            "required": ["n"]
        });
        assert!(validates(&json!({"n": 1}), &schema));
        assert!(!validates(&json!({"n": "not a number"}), &schema));
        assert!(!validates(&json!({}), &schema));
    }

    /// A broken schema must not reject an otherwise-good answer — compaction
    /// degrading because of a bad schema is worse than skipping validation.
    #[test]
    fn an_uncompilable_schema_does_not_reject_everything() {
        let bogus = json!({ "type": "not-a-real-type" });
        assert!(validates(&json!({"anything": true}), &bogus));
    }

    /// End-to-end with a real GGUF, opt-in via `KITTY_TEST_CHAT_GGUF`.
    ///
    /// This is the only check that the GBNF is actually accepted by
    /// llama.cpp and that a 1.2B model can satisfy a schema through the
    /// chain. A malformed grammar is rejected at sampler construction, which
    /// no unit test here would catch.
    #[tokio::test]
    async fn end_to_end_produces_schema_valid_json() {
        use adaptive_pathway::traits::StructuredChat;

        let Ok(path) = std::env::var("KITTY_TEST_CHAT_GGUF") else {
            eprintln!("skipping: set KITTY_TEST_CHAT_GGUF to a GGUF path to run");
            return;
        };
        let local = LocalEngineConfig {
            enabled: true,
            model_path: path,
            ..Default::default()
        };
        let s = LocalSummarizer::new(local, SummarizerConfig::default(), SlotManager::new());
        assert!(s.is_available());

        let schema = json!({
            "type": "object",
            "properties": {
                "sentiment": { "type": "string" },
                "confidence": { "type": "number" }
            },
            "required": ["sentiment"]
        });
        let out = s
            .structured_chat(
                vec![json!({
                    "role": "user",
                    "content": "Classify the sentiment of: 'I love this, it works perfectly.'"
                })],
                &schema,
            )
            .await
            .expect("chain should yield schema-valid JSON");

        assert!(out.is_object(), "expected an object, got {out}");
        assert!(
            out.get("sentiment").and_then(|v| v.as_str()).is_some(),
            "required key missing from {out}"
        );
    }

    #[tokio::test]
    async fn unconfigured_summarizer_reports_unavailable() {
        use adaptive_pathway::traits::StructuredChat;
        let s = LocalSummarizer::new(
            LocalEngineConfig::default(),
            SummarizerConfig::default(),
            SlotManager::new(),
        );
        assert!(!s.is_available());
        let err = s.structured_chat(vec![], &json!({})).await.unwrap_err();
        assert!(err.contains("not configured"), "got {err}");
    }
}
