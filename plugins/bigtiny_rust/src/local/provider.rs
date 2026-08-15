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
//! **Tool calls go through one protocol we impose, constrained by a grammar**
//! (see [`super::tools`]). The models this engine targets (~1.2B) can't be
//! trusted to emit a well-formed call unaided — the old code took that to mean
//! "no tools at all". The grammar changes the trade: any call the model does
//! emit is forced to name a real tool and carry a schema-valid argument
//! object, so a call is either impossible or correct, never plausibly-wrong.
//! Gated by `LocalEngineConfig.tool_calls` (on by default); when off, the
//! engine is text-only as before.

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;
use serde_json::Value;

use crate::config::LocalEngineConfig;
use crate::error::ProviderError;
use crate::provider::base::{Delta, HealthStatus, ModelInfo, Provider, SamplingParams, ToolCall};
use crate::provider::tag_split::TagSplitter;

/// A streaming assistant delta with the fields this engine never sets left
/// blank — `generate_blocking` builds a lot of these.
fn assistant_delta(
    content: Option<String>,
    reasoning: Option<String>,
    tool_calls: Option<Vec<ToolCall>>,
) -> Delta {
    Delta {
        role: "assistant".into(),
        content,
        reasoning,
        tool_calls,
        finish_reason: None,
        usage: None,
        error_type: None,
    }
}

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

/// Flatten agent-loop messages into the `(role, content)` pairs the chat
/// template consumes, folding in the tool protocol.
///
/// Pure and GGUF-free — this is where the tool-history round-trip is easy to
/// break, so it is the part unit tests can reach. Three things the naive
/// flatten (which just read `content`) got wrong the moment tools existed:
///
///   - an assistant turn that *called* a tool has empty `content` and its
///     calls in `tool_calls`; dropping those left the model blind to its own
///     prior actions. They are re-rendered as `<tool_call>…</tool_call>`, the
///     exact syntax it's asked to produce.
///   - a `role: "tool"` result has no rendering on most plain chat templates
///     (the C `apply_chat_template` errors or silently drops the unknown
///     role); it collapses to a `user` turn carrying `<tool_response>…`.
///   - the tool list rides on the leading system message (synthesized if the
///     conversation has none).
fn flatten_for_template(messages: &[Value], tools: &[Value]) -> Vec<(String, String)> {
    use super::tools;

    let text_content = |m: &Value| -> String {
        match m.get("content") {
            Some(Value::String(s)) => s.clone(),
            // Multimodal content arrives as an array of parts; keep the text
            // and drop the rest — this engine has no vision path.
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        }
    };

    let mut pairs: Vec<(String, String)> = Vec::with_capacity(messages.len());
    let mut injected_tools = false;

    for m in messages {
        let Some(role) = m.get("role").and_then(|r| r.as_str()) else {
            continue;
        };
        match role {
            "tool" => {
                // A tool *result* — becomes a user turn the template can render.
                let id = m.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("");
                let body = text_content(m);
                let rendered = format!(
                    "{}{{\"tool_call_id\": {:?}, \"content\": {:?}}}{}",
                    tools::RESPONSE_OPEN,
                    id,
                    body,
                    tools::RESPONSE_CLOSE
                );
                // Merge consecutive tool results into one user turn rather than
                // emitting a run of them, which some templates handle poorly.
                if let Some(last) = pairs.last_mut() {
                    if last.0 == "user" && last.1.starts_with(tools::RESPONSE_OPEN) {
                        last.1.push('\n');
                        last.1.push_str(&rendered);
                        continue;
                    }
                }
                pairs.push(("user".into(), rendered));
            }
            "assistant" => {
                let mut content = text_content(m);
                if let Some(calls) = m.get("tool_calls").and_then(|c| c.as_array()) {
                    for call in calls {
                        let f = call.get("function").unwrap_or(call);
                        let name = f.get("name").and_then(|n| n.as_str()).unwrap_or("");
                        let args = f
                            .get("arguments")
                            .cloned()
                            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(&format!(
                            "{}{{\"name\": {:?}, \"arguments\": {}}}{}",
                            tools::OPEN,
                            name,
                            args,
                            tools::CLOSE
                        ));
                    }
                }
                pairs.push(("assistant".into(), content));
            }
            "system" if !injected_tools && !tools.is_empty() => {
                injected_tools = true;
                let mut content = text_content(m);
                content.push_str(&tools::tool_system_block(tools));
                pairs.push(("system".into(), content));
            }
            _ => pairs.push((role.to_string(), text_content(m))),
        }
    }

    // No system message to hang the tool list on — prepend one.
    if !injected_tools && !tools.is_empty() {
        pairs.insert(
            0,
            (
                "system".into(),
                tools::tool_system_block(tools).trim_start().to_string(),
            ),
        );
    }

    pairs
}

/// Render `messages` through the model's own chat template.
///
/// Falls back to a plain role-tagged transcript only when the GGUF carries no
/// template at all — better than sending raw text, which reliably produces an
/// instant EOS on instruct models.
fn build_prompt(
    engine: &LocalEngine,
    messages: &[Value],
    tools: &[Value],
) -> Result<String, ProviderError> {
    use llama_cpp_2::model::LlamaChatMessage;

    let pairs = flatten_for_template(messages, tools);

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

/// Build the sampler chain from resolved [`SamplingParams`], optionally with a
/// lazy tool-call grammar as the first stage.
///
/// `temperature <= 0` means greedy — the deterministic path compaction wants.
/// Otherwise the order matters and mirrors llama.cpp's own recommended chain:
/// penalties → top_k → top_p → min_p → temp → dist.
///
/// The grammar goes *first* (it masks; the rest select among what's left) and
/// **into both branches** — leaving it out of the `temp <= 0` greedy path is
/// an easy miss that would silently drop tool constraints on a deterministic
/// turn. It is lazy (triggered on `<tool_call>`), so it costs nothing until
/// the model starts a call. A grammar llama.cpp rejects is a `warn!` and an
/// unconstrained chain, never a failed turn.
fn build_sampler(
    sampling: &SamplingParams,
    grammar: Option<(&llama_cpp_2::model::LlamaModel, &str)>,
) -> llama_cpp_2::sampling::LlamaSampler {
    use llama_cpp_2::sampling::LlamaSampler;

    let grammar_stage = grammar.and_then(|(model, gbnf)| {
        match LlamaSampler::grammar_lazy(model, gbnf, "root", [super::tools::OPEN], &[]) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("tool grammar was rejected ({e}); generating unconstrained");
                None
            }
        }
    });

    let temp = sampling.temperature.unwrap_or(0.8) as f32;
    if temp <= 0.0 {
        let mut stages = Vec::new();
        stages.extend(grammar_stage);
        stages.push(LlamaSampler::greedy());
        return LlamaSampler::chain_simple(stages);
    }

    let mut stages = Vec::new();
    stages.extend(grammar_stage);
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
    tools: Vec<Value>,
    grammar: Option<String>,
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

    let grammar_ref = grammar.as_deref().map(|g| (model, g));
    let mut sampler = build_sampler(&sampling, grammar_ref);
    let mut decoder = encoding_rs::UTF_8.new_decoder();
    // Absolute position of the next token — the whole prompt, not just the
    // final prefill chunk.
    let mut n_cur = prompt_tokens;
    let mut produced = 0i32;
    let mut finish = "stop";
    // Carries `<think>` state across pieces. The local engine emits one token
    // at a time, so the tag arrives as `<` / `think` / `>` far more often than
    // whole — that cross-piece bookkeeping is the whole point of the splitter.
    let mut think = TagSplitter::thinking();
    // Only scan for tool calls when tools are actually in play. When active,
    // the thinking splitter runs *first* and only its `outside` reaches the
    // scanner — a `<tool_call>` written inside a `<think>` block is the model
    // rehearsing, and must never execute.
    let mut scanner = (!tools.is_empty()).then(|| super::tools::ToolCallScanner::new(&tools));
    let mut emitted_tool_call = false;

    // Push one `ScanOut`'s worth of deltas; `false` if the receiver is gone.
    let emit_scanned = |scanned: super::tools::ScanOut,
                        emitted_tool_call: &mut bool|
     -> bool {
        if !scanned.text.is_empty()
            && tx.send(assistant_delta(Some(scanned.text), None, None)).is_err()
        {
            return false;
        }
        for call in scanned.calls {
            *emitted_tool_call = true;
            if tx
                .send(assistant_delta(None, None, Some(vec![call])))
                .is_err()
            {
                return false;
            }
        }
        for err in scanned.errors {
            // A rejected call is shown, not executed — so the user sees the
            // model attempted something invalid rather than a silent nothing.
            if tx
                .send(assistant_delta(Some(format!("\n[{err}]\n")), None, None))
                .is_err()
            {
                return false;
            }
        }
        true
    };

    while produced < max_new {
        let token = sampler.sample(&ctx, batch.n_tokens() - 1);
        sampler.accept(token);
        if model.is_eog_token(token) {
            break;
        }
        match model.token_to_piece(token, &mut decoder, true, None) {
            Ok(piece) if !piece.is_empty() => {
                // `<think>...</think>` → `reasoning`, everything else →
                // content (via the tool scanner when tools are active). Until
                // this the engine passed the tags through verbatim, so the
                // trace rendered as part of the answer and the Thinking box —
                // which keys off `reasoning` — never appeared.
                //
                // No model-name gate: a model that never emits the tag pays
                // one `str::find` per piece. Known and shared with the
                // openai_compat path: a literal `<think>` in a code fence is
                // swallowed.
                let split = think.feed(&piece);
                if !split.inside.is_empty()
                    && tx
                        .send(assistant_delta(None, Some(split.inside), None))
                        .is_err()
                {
                    // A closed receiver means the caller went away (cancelled
                    // turn, dropped stream) — stop burning CPU on it.
                    return;
                }
                if !split.outside.is_empty() {
                    match scanner.as_mut() {
                        Some(sc) => {
                            let scanned = sc.feed(&split.outside);
                            if !emit_scanned(scanned, &mut emitted_tool_call) {
                                return;
                            }
                        }
                        None => {
                            if tx
                                .send(assistant_delta(Some(split.outside), None, None))
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
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

    // Drain both splitters. `think.flush()` releases any held-back partial
    // `<think` tag as plain text, which still has to pass through the tool
    // scanner; then `scanner.flush()` releases its own unterminated span (a
    // `<tool_call>` cut off by the token ceiling) as visible text — never a
    // call.
    let think_tail = think.flush();
    let mut tail = String::new();
    if !think_tail.is_empty() {
        match scanner.as_mut() {
            Some(sc) => {
                let scanned = sc.feed(&think_tail);
                if !emit_scanned(scanned, &mut emitted_tool_call) {
                    return;
                }
            }
            None => tail.push_str(&think_tail),
        }
    }
    if let Some(sc) = scanner.as_mut() {
        tail.push_str(&sc.flush());
    }
    if !tail.is_empty() && tx.send(assistant_delta(Some(tail), None, None)).is_err() {
        return;
    }

    // The agent loop keys off the presence of tool calls, not the finish
    // string, but reporting `tool_calls` keeps timings/telemetry honest.
    if emitted_tool_call {
        finish = "tool_calls";
    }
    tracing::debug!(produced, finish, "local generation: done");

    // `input_tokens`/`output_tokens`, NOT the OpenAI wire names
    // `prompt_tokens`/`completion_tokens`. Those get normalized to these by
    // `openai_compat`'s response parser, but this provider bypasses that
    // entirely — so emitting the wire names meant every consumer
    // (`agent::loop_`'s stats, Kitty's `bigtiny::stream`) read a missing key
    // and reported 0 in / 0 out for every local turn.
    //
    // No `total_tokens` (nothing reads it) and deliberately no cache keys:
    // Kitty distinguishes an *absent* cache figure from a zero one, and this
    // engine has no prompt cache to report, so absent is the honest answer.
    let mut usage = std::collections::HashMap::new();
    usage.insert("input_tokens".to_string(), prompt_tokens);
    usage.insert("output_tokens".to_string(), produced);
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

    /// Tool calling is available unless the operator turned it off. The agent
    /// loop reads this to decide whether to send tools at all — see the module
    /// header and [`super::tools`].
    fn supports_tools(&self) -> bool {
        self.cfg.tool_calls
    }

    async fn chat_completion(
        &self,
        messages: Vec<Value>,
        tools: Option<Vec<Value>>,
        sampling: SamplingParams,
        _model: Option<String>,
        _id_slot: Option<i32>,
    ) -> Result<Pin<Box<dyn Stream<Item = Delta> + Send>>, ProviderError> {
        // Honor the config flag even if a caller sent tools anyway: the router
        // shouldn't, but a turn with `tool_calls` off must stay text-only.
        let tools: Vec<Value> = if self.cfg.tool_calls {
            tools.unwrap_or_default()
        } else {
            if tools.as_ref().is_some_and(|t| !t.is_empty()) {
                tracing::warn!(
                    provider = %self.provider_id,
                    "local tool calling is disabled; ignoring the tools on this turn"
                );
            }
            Vec::new()
        };

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

        let prompt = build_prompt(&engine, &messages, &tools)?;
        // Built once here (async side) rather than inside the blocking closure
        // so a schema llama.cpp rejects logs once per turn, not per token.
        let grammar = if tools.is_empty() {
            None
        } else {
            super::tools::tools_grammar(&tools)
        };

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::task::spawn_blocking(move || {
            generate_blocking(engine, prompt, sampling, tools, grammar, tx)
        });
        Ok(Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(
            rx,
        )))
    }

    async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        if self.cfg.model_path.trim().is_empty() {
            return Ok(vec![]);
        }
        // Report the context the engine actually resolved once the slot is
        // resident (fitted/estimated, `n_ctx_train`-clamped); before the
        // lazy first load, the static registration resolution. Never the
        // literal `0` "automatic" sentinel, which is what `cfg.n_ctx` holds
        // when the user asks for automatic sizing.
        let context_length = self
            .slots
            .get(SlotKind::Summarizer)
            .map(|e| e.effective_n_ctx())
            .unwrap_or_else(|| super::engine::registration_n_ctx(&self.cfg));
        Ok(vec![ModelInfo {
            id: self.model_label.clone(),
            name: Some(self.model_label.clone()),
            provider_id: Some(self.provider_id.clone()),
            context_length: Some(context_length as i32),
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

    // `flatten_for_template` is the pure, GGUF-free heart of the tool-history
    // round-trip, and the part most likely to silently drop something.

    #[test]
    fn flatten_without_tools_is_the_old_plain_transcript() {
        let msgs = vec![
            json!({ "role": "system", "content": "be brief" }),
            json!({ "role": "user", "content": "hi" }),
        ];
        let pairs = flatten_for_template(&msgs, &[]);
        assert_eq!(
            pairs,
            vec![
                ("system".to_string(), "be brief".to_string()),
                ("user".to_string(), "hi".to_string()),
            ]
        );
    }

    #[test]
    fn flatten_appends_the_tool_block_to_the_leading_system_message() {
        let tools = vec![json!({
            "type": "function",
            "function": { "name": "read_file", "description": "read", "parameters": {} }
        })];
        let msgs = vec![
            json!({ "role": "system", "content": "be brief" }),
            json!({ "role": "user", "content": "hi" }),
        ];
        let pairs = flatten_for_template(&msgs, &tools);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "system");
        assert!(pairs[0].1.starts_with("be brief"));
        assert!(pairs[0].1.contains("read_file"));
        assert!(pairs[0].1.contains("<tool_call>"));
    }

    #[test]
    fn flatten_synthesizes_a_system_turn_when_there_is_none() {
        let tools = vec![json!({ "function": { "name": "t" } })];
        let msgs = vec![json!({ "role": "user", "content": "hi" })];
        let pairs = flatten_for_template(&msgs, &tools);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "system");
        assert!(pairs[0].1.contains("<tool_call>"));
        assert_eq!(pairs[1], ("user".to_string(), "hi".to_string()));
    }

    #[test]
    fn flatten_round_trips_a_tool_call_and_its_result() {
        let tools = vec![json!({ "function": { "name": "read_file" } })];
        let msgs = vec![
            json!({ "role": "user", "content": "read a" }),
            // An assistant turn that called a tool: empty content, calls in
            // `tool_calls`. The naive flatten dropped this entirely.
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "read_file", "arguments": { "path": "a" } }
                }]
            }),
            json!({ "role": "tool", "tool_call_id": "call_1", "content": "hello" }),
        ];
        let pairs = flatten_for_template(&msgs, &tools);
        // system(synthesized) + user + assistant(call) + user(result)
        assert_eq!(pairs.len(), 4);
        let assistant = &pairs[2];
        assert_eq!(assistant.0, "assistant");
        assert!(assistant.1.contains("<tool_call>"));
        assert!(assistant.1.contains("read_file"));
        // The tool result became a user turn the template can render.
        let result = &pairs[3];
        assert_eq!(result.0, "user");
        assert!(result.1.contains("<tool_response>"));
        assert!(result.1.contains("hello"));
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
            // No grammar here — the grammar path needs a loaded model, so it's
            // exercised by the harness, not this pure unit test.
            let _ = build_sampler(&c, None);
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
        // The names every consumer actually reads — see the emit site.
        assert!(usage.get("output_tokens").copied().unwrap_or(0) > 0);
        assert!(usage.get("input_tokens").copied().unwrap_or(0) > 0);
        assert!(
            !usage.contains_key("prompt_tokens") && !usage.contains_key("completion_tokens"),
            "the OpenAI wire names are never normalized on this path"
        );
    }

    #[tokio::test]
    async fn discover_models_is_empty_when_unconfigured() {
        let p = LocalProvider::new("local", LocalEngineConfig::default(), SlotManager::new());
        assert!(p.discover_models().await.unwrap().is_empty());
    }

    /// Regression (815bugs #83): with `n_ctx = 0` ("automatic"),
    /// `discover_models` used to report `context_length: Some(0)` — the
    /// sentinel, not a size. Before the lazy first load it must report the
    /// static registration resolution (the same fallback the engine bottoms
    /// out at), never 0.
    #[tokio::test]
    async fn discover_models_never_reports_the_automatic_sentinel_as_zero() {
        let automatic = LocalEngineConfig {
            enabled: true,
            model_path: "no-such-model.gguf".into(),
            n_ctx: 0,
            ..Default::default()
        };
        let p = LocalProvider::new("local", automatic, SlotManager::new());
        let models = p.discover_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].context_length, Some(4096));

        // A pinned size advertises itself.
        let mut pinned_cfg = cfg("no-such-model.gguf");
        pinned_cfg.n_ctx = 8192;
        let p = LocalProvider::new("local", pinned_cfg, SlotManager::new());
        let models = p.discover_models().await.unwrap();
        assert_eq!(models[0].context_length, Some(8192));
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
