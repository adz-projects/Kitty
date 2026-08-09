//! Phase 2a probe for the local embedding backend (docs/ANDROID.md D4, §3.1
//! `embeddings.rs`).
//!
//! Validates, before `POST /api/embeddings` is built on top of it:
//!   1. the pinned embedding GGUF loads under `llama-cpp-2`,
//!   2. mean-pooled embeddings come back at the model's native dimension,
//!   3. the vectors are *semantically* meaningful — a related pair scores
//!      higher than an unrelated one. A backend that returns well-formed
//!      garbage (all-zeros, or a constant vector) passes a shape check but
//!      silently destroys adaptive-pathway recall, so shape alone is not
//!      enough evidence.
//!
//! Run:
//! ```text
//! cargo run --example local_embed_spike --features local-engine -- <embed.gguf>
//! ```

use std::num::NonZeroU32;
use std::path::PathBuf;

use llama_cpp_2::context::params::{LlamaContextParams, LlamaPoolingType};
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};

const N_CTX: u32 = 512;

/// Qwen3-Embedding is a causal-LM-derived embedder and pools on the **last**
/// token, per its model card. This has to be set explicitly: llama.cpp's
/// default here is `None`, which produces no sequence embedding at all —
/// `embeddings_seq_ith` then fails with `NonePoolType` rather than returning
/// something subtly wrong, which at least fails loudly. A different embedder
/// (BERT-style: bge, gte, nomic) would want `Mean` or `Cls`, so this belongs
/// alongside the model pin, not hardcoded in the engine.
const POOLING: LlamaPoolingType = LlamaPoolingType::Last;

fn embed(
    model: &LlamaModel,
    backend: &LlamaBackend,
    text: &str,
) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
    let params = LlamaContextParams::default()
        .with_n_ctx(NonZeroU32::new(N_CTX))
        .with_embeddings(true)
        .with_pooling_type(POOLING);
    let mut ctx = model.new_context(backend, params)?;

    let tokens = model.str_to_token(text, AddBos::Always)?;
    let mut batch = LlamaBatch::new(tokens.len().max(1), 1);
    for (i, token) in (0i32..).zip(tokens) {
        batch.add(token, i, &[0], true)?;
    }
    // Embedding models are encoder-style: `decode` still drives the graph and
    // populates the embedding output for sequence 0.
    ctx.decode(&mut batch)?;

    let raw = ctx.embeddings_seq_ith(0)?;
    // L2-normalise so cosine reduces to a dot product, matching what
    // adaptive-pathway's vector ops expect.
    let norm: f32 = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
    Ok(if norm > 1e-12 {
        raw.iter().map(|x| x / norm).collect()
    } else {
        raw.to_vec()
    })
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let model_path = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("usage: local_embed_spike <embed.gguf>")?,
    );
    if !model_path.is_file() {
        return Err(format!("model not found: {}", model_path.display()).into());
    }

    let backend = LlamaBackend::init()?;
    let model = LlamaModel::load_from_file(&backend, &model_path, &LlamaModelParams::default())?;
    eprintln!(
        "embed model loaded: n_embd {}, n_ctx_train {}, layers {}",
        model.n_embd(),
        model.n_ctx_train(),
        model.n_layer()
    );

    let a = embed(&model, &backend, "The cat sat on the warm windowsill.")?;
    let b = embed(&model, &backend, "A kitten napped in the sunny window.")?;
    let c = embed(&model, &backend, "Quarterly amortisation of deferred tax assets.")?;

    println!("dim            = {}", a.len());
    let related = cosine(&a, &b);
    let unrelated = cosine(&a, &c);
    println!("cos(related)   = {related:.4}");
    println!("cos(unrelated) = {unrelated:.4}");

    if a.len() != model.n_embd() as usize {
        return Err(format!("dim {} != n_embd {}", a.len(), model.n_embd()).into());
    }
    if a.iter().all(|x| *x == 0.0) {
        return Err("all-zero embedding — backend produced nothing usable".into());
    }
    // The real check: the space has to separate meaning, not just have a shape.
    if related <= unrelated {
        return Err(format!(
            "embeddings are not semantically ordered (related {related:.4} <= unrelated {unrelated:.4})"
        )
        .into());
    }
    println!("OK: {}-dim, semantically ordered", a.len());
    Ok(())
}
