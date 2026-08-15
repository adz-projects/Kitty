//! Local embedding backend (docs/ANDROID.md §3.1 `embeddings.rs`, D4 revised).
//!
//! Since D4's revision this runs on **both** platforms with the same model, so
//! beliefs embedded on desktop and on Android live in one comparable vector
//! space.
//!
//! The wire shape it serves is deliberately **Ollama-compatible**
//! (`{model, prompt}` → `{embedding: [...]}`). That is not nostalgia: it makes
//! Phase 2b's adaptive-pathway re-point a one-line base-URL change in
//! `src-tauri/src/lifecycle/bigtiny_proc.rs` instead of a rename across ~40
//! call sites in the AP crate. See §10 Phase 2b.

use super::engine::{EmbedPooling, LocalEngine, LocalEngineError};

/// Embed one string, returning an L2-normalised vector.
///
/// Normalising here (rather than at each call site) means cosine similarity
/// reduces to a dot product, which is what adaptive-pathway's vector ops
/// already assume.
///
/// Blocking: builds a context and runs a forward pass. Call under
/// `spawn_blocking` from async code.
pub fn embed_one(
    engine: &LocalEngine,
    pooling: EmbedPooling,
    text: &str,
) -> Result<Vec<f32>, LocalEngineError> {
    use llama_cpp_2::llama_batch::LlamaBatch;
    use llama_cpp_2::model::AddBos;

    let trimmed = text.trim();
    if trimmed.is_empty() {
        // A zero vector is the honest answer for empty input, and matches
        // what adaptive-pathway's own provider does for the same case.
        return Ok(vec![0.0; engine.n_embd().max(0) as usize]);
    }

    let mut ctx = engine.embedding_context(pooling)?;
    let model = engine.model();

    let mut tokens = model
        .str_to_token(trimmed, AddBos::Always)
        .map_err(|e| LocalEngineError::Inference(format!("tokenize failed: {e}")))?;
    if tokens.is_empty() {
        return Ok(vec![0.0; engine.n_embd().max(0) as usize]);
    }

    // A pooled embedding needs the whole sequence in one batch, so this can't
    // be chunked the way generation prefill is — instead cap the input to what
    // the embedding context's batch can hold. Submitting more than `n_batch`
    // tokens makes llama.cpp *abort the process* (`GGML_ASSERT(n_tokens_all <=
    // cparams.n_batch)`), which would take the whole daemon down; truncating an
    // over-long belief snippet is a benign degradation by comparison. The cap
    // is also `<= embed_n_ctx`, so it never exceeds the context either.
    let cap = (ctx.n_batch() as usize).max(1);
    if tokens.len() > cap {
        tracing::debug!(
            tokens = tokens.len(),
            cap,
            "embedding input exceeds the embed context batch; truncating"
        );
        tokens.truncate(cap);
    }

    let mut batch = LlamaBatch::new(tokens.len(), 1);
    for (i, token) in (0i32..).zip(tokens) {
        // Pooled embeddings need logits on every token, not just the last.
        batch
            .add(token, i, &[0], true)
            .map_err(|e| LocalEngineError::Inference(format!("batch add failed: {e}")))?;
    }
    ctx.decode(&mut batch)
        .map_err(|e| LocalEngineError::Inference(format!("decode failed: {e}")))?;

    let raw = ctx
        .embeddings_seq_ith(0)
        .map_err(|e| LocalEngineError::Inference(format!("no sequence embedding: {e}")))?;

    Ok(l2_normalise(raw))
}

fn l2_normalise(v: &[f32]) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-12 {
        v.iter().map(|x| x / norm).collect()
    } else {
        // Degenerate output — hand it back as-is rather than dividing by ~0
        // and producing NaNs, which would poison every later cosine.
        v.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalisation_yields_unit_length() {
        let out = l2_normalise(&[3.0, 4.0]);
        let len: f32 = out.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((len - 1.0).abs() < 1e-6, "got {len}");
    }

    /// An all-zero vector must survive rather than becoming NaN — a single
    /// NaN would silently contaminate every downstream cosine comparison.
    #[test]
    fn all_zero_input_does_not_produce_nan() {
        let out = l2_normalise(&[0.0, 0.0, 0.0]);
        assert!(out.iter().all(|x| x.is_finite()));
        assert_eq!(out, vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn normalisation_preserves_direction() {
        let out = l2_normalise(&[0.0, 2.0]);
        assert!(out[0].abs() < 1e-6);
        assert!((out[1] - 1.0).abs() < 1e-6);
    }
}
