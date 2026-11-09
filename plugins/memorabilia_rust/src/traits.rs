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

/// Optional judge of whether a document is worth remembering at all, run
/// right after the Stage 0 PII gate. BigTiny supplies its per-app decision
/// model here. `Some(p)` rejects the document (`p` = the judged probability it
/// is worth keeping, for the audit row); `None` -- including any failure --
/// lets it through, exactly as if no gate were installed.
#[async_trait]
pub trait WorthinessGate: Send + Sync {
    async fn reject(&self, text: &str) -> Option<f32>;
}

/// Test double for [`WorthinessGate`]: rejects every document with `p` when
/// set.
#[derive(Debug, Clone, Default)]
pub struct MockWorthinessGate {
    pub reject_with: Option<f32>,
}

#[async_trait]
impl WorthinessGate for MockWorthinessGate {
    async fn reject(&self, _text: &str) -> Option<f32> {
        self.reject_with
    }
}

/// What the paragraph screen said about one paragraph
/// (docs/MEMORY_OVERNIGHT.md §2).
#[derive(Debug, Clone, PartialEq)]
pub enum ScreenVerdict {
    /// No screen configured, or the app switched it off: the pre-filter
    /// alone decides.
    Disabled,
    /// The screen is on but could not answer (endpoint down, too slow). The
    /// document waits, unscreened, for it to come back.
    Unavailable,
    /// Every question's P(yes), by name, whether the paragraph cleared the
    /// keep thresholds, and which intake lane kept it (`current` /
    /// `durable`; `None` for a set with no lanes).
    Scored { keep: bool, scores: Vec<(String, f32)>, lane: Option<String> },
}

/// Judges one paragraph at a time for the overnight intake. BigTiny supplies
/// the app's decision model here; the questions it asks are the app's
/// configured question set.
#[async_trait]
pub trait ParagraphScreen: Send + Sync {
    /// Whether the screen is configured and switched on right now. `false`
    /// means "decide without it", never "wait for it".
    async fn enabled(&self) -> bool;
    /// Judge `state` (the paragraph with its document context).
    async fn screen(&self, state: &str) -> ScreenVerdict;
    /// Identifies the question set in use, for the screen log.
    async fn question_set(&self) -> String;

    /// How many paragraphs to put in one model call. `1` (the default) is one
    /// call per paragraph — the calibrated behavior. A larger value batches
    /// via [`Self::screen_batch`], which is cheaper but whose accuracy must be
    /// measured with the eval first (docs/MEMORY_OVERNIGHT.md), so it is
    /// opt-in. The overnight intake groups screenable paragraphs by this.
    async fn batch_size(&self) -> usize {
        1
    }

    /// Judge several paragraphs, verdicts returned in `states` order. The
    /// default screens them one at a time (identical to calling [`Self::screen`]
    /// on each), so a backend that does not override it batches safely as
    /// groups of one. A backend may override it to combine the paragraphs into
    /// a single model call.
    async fn screen_batch(&self, states: &[&str]) -> Vec<ScreenVerdict> {
        let mut out = Vec::with_capacity(states.len());
        for s in states {
            out.push(self.screen(s).await);
        }
        out
    }
}

/// Test double for [`ParagraphScreen`]: keeps a paragraph when it contains
/// any of `keep_if`, or answers with `fixed` when set.
#[derive(Debug, Clone, Default)]
pub struct MockParagraphScreen {
    pub enabled: bool,
    pub fixed: Option<ScreenVerdict>,
    pub keep_if: Vec<String>,
}

#[async_trait]
impl ParagraphScreen for MockParagraphScreen {
    async fn enabled(&self) -> bool {
        self.enabled
    }
    async fn screen(&self, state: &str) -> ScreenVerdict {
        if let Some(v) = &self.fixed {
            return v.clone();
        }
        let keep = self.keep_if.iter().any(|k| state.contains(k.as_str()));
        ScreenVerdict::Scored {
            keep,
            scores: vec![("mock".into(), if keep { 0.9 } else { 0.1 })],
            lane: None,
        }
    }
    async fn question_set(&self) -> String {
        "mock".into()
    }
}
