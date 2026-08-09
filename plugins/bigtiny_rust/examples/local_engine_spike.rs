//! Phase 1 cross-compile spike (docs/ANDROID.md §10 Phase 1).
//!
//! Proves three things before any product code is written against
//! `llama-cpp-2`:
//!   1. the crate links and its cmake build of llama.cpp succeeds,
//!   2. the pinned default model's `lfm2` architecture is actually recognized
//!      by the vendored llama.cpp (§9 — if it isn't, the doc's fallback to
//!      Qwen3-1.2B gets tombstoned there),
//!   3. a context can be built and tokens sampled end to end.
//!
//! Deliberately NOT wired into the daemon — Phase 2a builds the real
//! `LocalEngine`/`LocalProvider` on top of what this validates. Keep it
//! dependency-light (no clap/hf-hub/encoding_rs) so it stays a linkage probe
//! rather than a second implementation to maintain.
//!
//! Run:
//! ```text
//! cargo run --example local_engine_spike --features local-engine -- <path.gguf> [prompt]
//! ```

use std::io::Write;
use std::num::NonZeroU32;
use std::path::PathBuf;

use encoding_rs::UTF_8;

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;

/// Keep the probe short — we're proving tokens flow, not benchmarking.
const MAX_TOKENS: i32 = 48;
const DEFAULT_PROMPT: &str = "In one sentence, what is a cat?";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let model_path = PathBuf::from(
        args.next()
            .ok_or("usage: local_engine_spike <path.gguf> [prompt]")?,
    );
    let prompt = args.next().unwrap_or_else(|| DEFAULT_PROMPT.to_string());

    if !model_path.is_file() {
        return Err(format!("model not found: {}", model_path.display()).into());
    }

    let backend = LlamaBackend::init()?;
    eprintln!("backend initialised");

    // (2) The load itself is the architecture check: an unsupported arch fails
    // here rather than at generation time.
    let model = LlamaModel::load_from_file(&backend, &model_path, &LlamaModelParams::default())?;
    eprintln!(
        "model loaded: {} layers, n_embd {}, n_ctx_train {}, vocab {}",
        model.n_layer(),
        model.n_embd(),
        model.n_ctx_train(),
        model.n_vocab(),
    );

    // Small context: this is a linkage probe, not a real session.
    let ctx_params = LlamaContextParams::default().with_n_ctx(NonZeroU32::new(2048));
    let mut ctx = model.new_context(&backend, ctx_params)?;

    // LFM2.5 is instruct-tuned: feeding a bare string makes it emit EOS
    // immediately. Apply the model's own chat template (the real
    // `LocalProvider` will do the same) and fall back to the raw prompt only
    // if the GGUF carries no template.
    let templated = match model.chat_template(None) {
        Ok(tmpl) => {
            let chat = vec![LlamaChatMessage::new("user".into(), prompt.clone())?];
            let s = model.apply_chat_template(&tmpl, &chat, true)?;
            eprintln!("chat template applied ({} chars)", s.len());
            s
        }
        Err(e) => {
            eprintln!("no chat template in GGUF ({e}); using the raw prompt");
            prompt.clone()
        }
    };

    // The template already emits the model's BOS/turn markers, so don't add a
    // second BOS on top of it.
    let tokens = model.str_to_token(&templated, AddBos::Never)?;
    eprintln!("prompt tokenised to {} tokens", tokens.len());

    let mut batch = LlamaBatch::new(512, 1);
    let last = tokens.len() as i32 - 1;
    for (i, token) in (0i32..).zip(tokens) {
        // Only the final prompt token needs logits.
        batch.add(token, i, &[0], i == last)?;
    }
    ctx.decode(&mut batch)?;

    let mut sampler = LlamaSampler::chain_simple([LlamaSampler::greedy()]);
    let mut n_cur = batch.n_tokens();
    let mut produced = 0;
    // `token_to_piece` (the non-deprecated path) decodes incrementally, so a
    // multi-byte UTF-8 codepoint split across two tokens still renders.
    let mut decoder = UTF_8.new_decoder();

    print!("\n--- output ---\n");
    while produced < MAX_TOKENS {
        let token = sampler.sample(&ctx, batch.n_tokens() - 1);
        sampler.accept(token);
        if model.is_eog_token(token) {
            // Say *why* we stopped — an immediate stop token is the signature
            // of a bad prompt shape, and looks identical to a broken build if
            // it's silent.
            eprintln!("\n[stop token {token:?} after {produced} tokens]");
            break;
        }
        print!("{}", model.token_to_piece(token, &mut decoder, true, None)?);
        std::io::stdout().flush()?;

        batch.clear();
        batch.add(token, n_cur, &[0], true)?;
        n_cur += 1;
        produced += 1;
        ctx.decode(&mut batch)?;
    }

    println!("\n--- end ({produced} tokens) ---");
    if produced == 0 {
        return Err("model loaded but produced no tokens".into());
    }
    Ok(())
}
