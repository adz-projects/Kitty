//! Dependency-inversion seams (project-plan.md §13.1). The engine depends
//! only on these traits, never on concrete providers — the same engine runs
//! under tests (`MockChat` + deterministic hash embedder), the daemon, and
//! any future host. This is the one seam that makes the whole system
//! testable without a live network.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

/// A JSON-schema-constrained completion interface for the extraction LLM
/// (Stage 4). Production implements it over a local provider (llama.cpp /
/// Ollama — never the calling LLM, per plan §3.6); tests use [`MockChat`].
#[async_trait]
pub trait StructuredChat: Send + Sync {
    /// Request a structured completion. Returns the parsed JSON content, or
    /// an error for the caller to treat as "this pass is skipped" (never a
    /// hard failure — it is logged and the chunk stays `pending` for retry,
    /// plan §3.6).
    async fn structured_chat(
        &self,
        messages: Vec<Value>,
        schema: &Value,
    ) -> Result<Value, String>;
}

/// A mock `StructuredChat` for tests that returns a canned response.
pub struct MockChat {
    pub response: Value,
}

#[async_trait]
impl StructuredChat for MockChat {
    async fn structured_chat(
        &self,
        _messages: Vec<Value>,
        _schema: &Value,
    ) -> Result<Value, String> {
        Ok(self.response.clone())
    }
}

/// Embedding provider seam (plan §13.1). Production: the pinned semantic
/// model with the deterministic lexical signed-hash fallback; tests: the
/// hash embedder directly, so cosine thresholds, clustering, and priority
/// ordering are reproducible offline.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embed one text, returning the vector and the tag of the vector space
    /// that *actually* produced it (plan §3.3). A fallback-aware embedder
    /// may return [`config::HASH_EMBED_MODEL`] even though its configured
    /// primary is the semantic model — the two spaces are incompatible, so
    /// callers persist and filter by the returned tag, never by the
    /// embedder's static configuration. Errors are returned for the caller
    /// to log-and-swallow, never raised as a hard failure.
    async fn embed(&self, text: &str) -> Result<(Vec<f32>, String), String>;

    /// Embed without consulting the cache (plan §3.3 `reembed_stale`). Cache
    /// hits for text embedded during a prior outage would otherwise keep
    /// returning the stale hash-fallback vector forever; the re-embed pass
    /// needs a genuine retry. Default: same as [`Self::embed`] (an
    /// embedder with no cache has no stale entries).
    async fn embed_fresh(&self, text: &str) -> Result<(Vec<f32>, String), String> {
        self.embed(text).await
    }

    /// Tag of the vector space this embedder's *primary* path produces
    /// (plan §3.3): the pinned semantic model in production,
    /// [`config::HASH_EMBED_MODEL`] for the fallback. Per-call results may
    /// carry a different tag (see [`Self::embed`]).
    fn model_tag(&self) -> &str;
}

/// Brute-force vector index over chunk embeddings (plan §3.3: SQLite +
/// sqlite-vec, cosine over *active* chunks; HNSW is a documented scale-out
/// path, not the default).
#[async_trait]
pub trait VectorIndex: Send + Sync {
    /// Insert or replace the embedding for `chunk_id`, tagged with the
    /// `model` that produced it (plan §3.3).
    async fn upsert(
        &self,
        chunk_id: &str,
        embedding: &[f32],
        model: &str,
    ) -> Result<(), String>;

    /// Remove `chunk_id`'s vector (chunks drop their rows atomically when
    /// archived/deleted, plan §3.3).
    async fn remove(&self, chunk_id: &str) -> Result<(), String>;

    /// Top-`k` cosines against `query`, over vectors tagged exactly `model`
    /// — a lexical-fallback vector must never cosine-compare against a
    /// semantic query (plan §3.3, regression guard).
    ///
    /// Returns `(chunk_id, cosine)` pairs, best first.
    async fn search(
        &self,
        query: &[f32],
        model: &str,
        k: usize,
    ) -> Result<Vec<(String, f32)>, String>;

    /// Chunk ids whose vectors were produced by `model` — used by the
    /// `reembed_stale` heavy pass to find lexical-fallback rows once the
    /// semantic service has recovered (plan §3.3 / §11).
    async fn list_by_model(&self, model: &str) -> Result<Vec<String>, String>;
}

/// Brute-force in-memory `VectorIndex` — the test double for the seam,
/// alongside [`MockChat`]. Implements the seam's contract exactly (exact
/// model-tag filter, cosine over stored rows, deterministic order) with no
/// logic beyond it, so the `__lexical_hash__` / semantic cross-space guard
/// (plan §3.3, §13.6) is pinned against a real `VectorIndex` implementation
/// from Phase 1.
#[derive(Default)]
pub struct MemoryVectorIndex {
    /// `chunk_id` → (vector, producing model tag).
    inner: Mutex<HashMap<String, (Vec<f32>, String)>>,
}

impl MemoryVectorIndex {
    pub fn new() -> Self {
        Self::default()
    }
}

fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

#[async_trait]
impl VectorIndex for MemoryVectorIndex {
    async fn upsert(
        &self,
        chunk_id: &str,
        embedding: &[f32],
        model: &str,
    ) -> Result<(), String> {
        self.inner
            .lock()
            .unwrap()
            .insert(chunk_id.to_string(), (embedding.to_vec(), model.to_string()));
        Ok(())
    }

    async fn remove(&self, chunk_id: &str) -> Result<(), String> {
        self.inner.lock().unwrap().remove(chunk_id);
        Ok(())
    }

    async fn search(
        &self,
        query: &[f32],
        model: &str,
        k: usize,
    ) -> Result<Vec<(String, f32)>, String> {
        let guard = self.inner.lock().unwrap();
        let qn = norm(query);
        let mut scored: Vec<(String, f32)> = Vec::new();
        if qn > 0.0 {
            for (id, (v, m)) in guard.iter() {
                // Exact-space filter: a vector from another model is in a
                // different space and is never compared (plan §3.3).
                if m != model || v.len() != query.len() {
                    continue;
                }
                let vn = norm(v);
                if vn > 0.0 {
                    let dot: f32 = query.iter().zip(v.iter()).map(|(a, b)| a * b).sum();
                    scored.push((id.clone(), dot / (qn * vn)));
                }
            }
        }
        // Cosine descending; chunk id breaks ties so repeated calls agree
        // byte-for-byte (deterministic ranking, plan §13.4).
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        scored.truncate(k);
        Ok(scored)
    }

    async fn list_by_model(&self, model: &str) -> Result<Vec<String>, String> {
        let guard = self.inner.lock().unwrap();
        let mut ids: Vec<String> = guard
            .iter()
            .filter(|(_, (_, m))| m == model)
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        Ok(ids)
    }
}
