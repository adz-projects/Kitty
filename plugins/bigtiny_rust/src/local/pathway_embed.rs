//! Adaptive-pathway's semantic embedder, served in-process
//! (docs/ANDROID.md §10 Phase 2b).
//!
//! The pathway engine is *linked into this binary*, so the obvious reading of
//! "point AP at the daemon's `/api/embeddings`" would have the daemon issuing
//! an HTTP request to its own listener: a socket round-trip, a second copy of
//! the vector, and a request that has to satisfy the API-key middleware the
//! daemon applies to every `/api/*` route but `/api/health` (mandatory on
//! Android per D25). None of that buys anything — the slot manager holding the
//! model is one struct field away.
//!
//! `POST /api/embeddings` stays exactly as it is, for out-of-process callers
//! and `tools/local_engine_lab.py`. This is a second entrance to the same
//! model, not a replacement.

use std::sync::Arc;

use adaptive_pathway::embed::SemanticEmbedder;
use async_trait::async_trait;

use crate::config::LocalEngineConfig;

use super::embeddings::embed_one;
use super::engine::EmbedPooling;
use super::manager::{SlotKind, SlotManager};

pub struct LocalPathwayEmbedder {
    slots: SlotManager,
    cfg: LocalEngineConfig,
    pooling: EmbedPooling,
}

impl LocalPathwayEmbedder {
    pub fn new(slots: SlotManager, cfg: LocalEngineConfig) -> Self {
        let pooling = EmbedPooling::parse(&cfg.embed_pooling);
        Self {
            slots,
            cfg,
            pooling,
        }
    }

    /// The vector space's identity, as adaptive-pathway records it on every
    /// belief (`Config::embedding.ollama_model`).
    ///
    /// This is compared, not displayed: `list_recall_candidates` filters on it
    /// and `sync_embedding_model_fingerprint` diffs it against what's on disk,
    /// so it must change whenever the weights do. Deriving it from the GGUF's
    /// filename gives that for free — swap the file, and existing beliefs are
    /// correctly marked stale for `reembed_stale_beliefs` to migrate rather
    /// than silently compared across two incompatible spaces.
    pub fn space_tag(embed_model_path: &str) -> String {
        let stem = std::path::Path::new(embed_model_path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_string());
        format!("local:{stem}")
    }
}

#[async_trait]
impl SemanticEmbedder for LocalPathwayEmbedder {
    async fn embed(&self, text: &str) -> Option<Vec<f32>> {
        let slots = self.slots.clone();
        let cfg = self.cfg.clone();
        let pooling = self.pooling;
        let text = text.to_string();

        // Both the model load and the forward pass are blocking and CPU-bound.
        let joined = tokio::task::spawn_blocking(move || {
            let engine = slots.get_or_load(SlotKind::Embedder, &cfg)?;
            embed_one(&engine, pooling, &text)
        })
        .await;

        match joined {
            Ok(Ok(v)) => Some(v),
            Ok(Err(e)) => {
                // Log it: AP's only signal is `None`, which it treats as
                // "unavailable, use hash space" — correct behaviour, but it
                // makes a misconfigured path look like a quality problem
                // rather than a broken setting.
                tracing::warn!("local embedding failed: {e}");
                None
            }
            Err(join) => {
                tracing::error!("local embedding task panicked: {join}");
                None
            }
        }
    }
}

/// Build the embedder for `cfg`, or `None` when the local engine can't serve
/// one — AP then keeps its existing behaviour (HTTP to a real Ollama if one is
/// configured, lexical hashing otherwise).
pub fn embedder_for(
    slots: &SlotManager,
    cfg: &LocalEngineConfig,
) -> Option<Arc<dyn SemanticEmbedder>> {
    if !cfg.enabled || cfg.embed_model_path.trim().is_empty() {
        return None;
    }
    Some(Arc::new(LocalPathwayEmbedder::new(
        slots.clone(),
        cfg.clone(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_space_tag_follows_the_gguf_filename() {
        assert_eq!(
            LocalPathwayEmbedder::space_tag("C:/models/Qwen3-Embedding-0.6B-q4_k_m.gguf"),
            "local:Qwen3-Embedding-0.6B-q4_k_m"
        );
        // Two different files must never collide, or a model swap would go
        // undetected and mix vector spaces.
        assert_ne!(
            LocalPathwayEmbedder::space_tag("/m/bge-small-en-v1.5.gguf"),
            LocalPathwayEmbedder::space_tag("/m/nomic-embed-text.gguf")
        );
    }

    /// The `local:` prefix keeps these from ever colliding with an Ollama tag
    /// (`qwen3-embedding:0.6b`) — the two are different vector spaces even
    /// when they're nominally the same model, since quantisation differs.
    #[test]
    fn the_space_tag_is_namespaced_away_from_ollama_tags() {
        assert!(LocalPathwayEmbedder::space_tag("x/qwen3-embedding.gguf").starts_with("local:"));
    }

    /// An unconfigured or disabled engine must hand back `None` so AP falls
    /// through to its own path, rather than an embedder that fails on every
    /// call and trips the circuit-breaker.
    #[test]
    fn no_embedder_when_the_engine_is_disabled_or_unconfigured() {
        let slots = SlotManager::new();
        let disabled = LocalEngineConfig {
            enabled: false,
            embed_model_path: "x.gguf".into(),
            ..Default::default()
        };
        assert!(embedder_for(&slots, &disabled).is_none());

        let unconfigured = LocalEngineConfig {
            enabled: true,
            embed_model_path: "   ".into(),
            ..Default::default()
        };
        assert!(embedder_for(&slots, &unconfigured).is_none());

        let ok = LocalEngineConfig {
            enabled: true,
            embed_model_path: "x.gguf".into(),
            ..Default::default()
        };
        assert!(embedder_for(&slots, &ok).is_some());
    }
}
