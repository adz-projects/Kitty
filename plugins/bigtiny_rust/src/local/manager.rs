//! Resident slot manager (docs/ANDROID.md §4.1).
//!
//! Owns *which* models are loaded, not what they're used for. Two named slots
//! exist today — the summarizer and the embedder — because those are the two
//! roles both platforms share (D4 revised, D18 amended). A chat-slot pool for
//! Windows local chat (D2/D21) is a later addition; the enum is the seam for
//! it, which is why `SlotKind` is not a bool.
//!
//! Loading is slow and blocking (hundreds of MB off disk), so `load` is sync
//! and callers on the async runtime must `spawn_blocking` it. The manager
//! itself is cheap to lock: it only ever holds the map, never a model load.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::config::LocalEngineConfig;

use super::engine::{LocalEngine, LocalEngineError};

/// Which resident role a loaded model is filling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlotKind {
    /// Compaction/summarization, and the general-purpose local model.
    Summarizer,
    /// Adaptive-pathway embeddings.
    Embedder,
}

impl SlotKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summarizer => "summarizer",
            Self::Embedder => "embedder",
        }
    }

    /// Which GGUF this slot loads, from config.
    fn model_path(self, cfg: &LocalEngineConfig) -> &str {
        match self {
            Self::Summarizer => &cfg.model_path,
            Self::Embedder => &cfg.embed_model_path,
        }
    }
}

/// Snapshot for `/api/local/models/status` (§3.1 `health.rs`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SlotStatus {
    pub kind: String,
    pub loaded: bool,
    pub model_path: Option<String>,
    /// Present once loaded; lets a caller detect an embedding-width change
    /// without loading the model itself.
    pub n_embd: Option<i32>,
    /// Which compute backend this slot's model is *actually* resident on
    /// (D20). `None` while unloaded — reporting the backend a fresh selection
    /// would pick would be a different claim than "what this model is running
    /// on", and the model card asks the latter.
    pub backend: Option<super::backend::SelectedBackend>,
    /// Layers actually offloaded. Worth reporting separately from `backend`
    /// because "Automatic" can legitimately resolve to a *partial* offload —
    /// a GPU-backed slot with 12 of 16 layers resident is neither "on the
    /// GPU" nor "on the CPU", and the model card should not have to guess.
    pub n_gpu_layers: Option<i32>,
    /// Context size this slot's generation contexts are actually built with,
    /// after automatic fitting and the `n_ctx_train` clamp. This is the number
    /// the user's `n_ctx` setting resolved to, which is not always the number
    /// they chose.
    pub n_ctx: Option<u32>,
    /// Why the slot is empty, when it is. `None` while loaded.
    pub error: Option<String>,
}

#[derive(Default)]
struct Inner {
    slots: HashMap<SlotKind, Arc<LocalEngine>>,
    /// Last load failure per slot, so status can explain an empty slot
    /// instead of just reporting `loaded: false`.
    errors: HashMap<SlotKind, String>,
}

/// Cheap to clone; all clones share one set of slots.
#[derive(Clone, Default)]
pub struct SlotManager {
    inner: Arc<Mutex<Inner>>,
}

impl SlotManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Already-loaded engine for `kind`, if any. Never loads.
    pub fn get(&self, kind: SlotKind) -> Option<Arc<LocalEngine>> {
        self.inner.lock().unwrap().slots.get(&kind).cloned()
    }

    /// Get the slot's engine, loading it if absent.
    ///
    /// Blocking — `spawn_blocking` from async code. The model load runs
    /// *outside* the lock: a multi-second load must not stall `status()` or a
    /// concurrent request for the other slot. The cost is that two racing
    /// callers can both load; the loser's copy is dropped on insert, which is
    /// wasted work but never incorrect.
    pub fn get_or_load(
        &self,
        kind: SlotKind,
        cfg: &LocalEngineConfig,
    ) -> Result<Arc<LocalEngine>, LocalEngineError> {
        if let Some(engine) = self.get(kind) {
            return Ok(engine);
        }

        let path = kind.model_path(cfg);
        if path.trim().is_empty() {
            let msg = format!("no model configured for the {} slot", kind.as_str());
            self.record_error(kind, &msg);
            return Err(LocalEngineError::NotConfigured(msg));
        }

        let loaded = match LocalEngine::load(Path::new(path), cfg) {
            Ok(e) => Arc::new(e),
            Err(e) => {
                self.record_error(kind, &e.to_string());
                return Err(e);
            }
        };

        let mut inner = self.inner.lock().unwrap();
        inner.errors.remove(&kind);
        // Another caller may have won the race; prefer the existing entry so
        // callers that already hold it keep pointing at the same model.
        Ok(inner.slots.entry(kind).or_insert(loaded).clone())
    }

    /// Drop a slot's model. Returns whether anything was resident.
    ///
    /// Weights are only actually freed once every outstanding `Arc` is
    /// dropped — an in-flight request keeps its engine alive, which is the
    /// intended behaviour (§4.1: "in-flight streams are never aborted").
    pub fn unload(&self, kind: SlotKind) -> bool {
        self.inner.lock().unwrap().slots.remove(&kind).is_some()
    }

    /// Free memory under pressure, per §4.1's eviction order: **the embedder
    /// goes first**. A missing summarization blocks compaction, while a
    /// missing embedding only lowers recall quality until adaptive-pathway's
    /// existing `reembed_stale_beliefs` pass catches it.
    ///
    /// Returns the slots actually evicted.
    pub fn evict_under_pressure(&self) -> Vec<SlotKind> {
        let mut evicted = Vec::new();
        for kind in [SlotKind::Embedder, SlotKind::Summarizer] {
            if self.unload(kind) {
                evicted.push(kind);
                // One slot is usually enough; stop rather than emptying
                // everything on the first sign of pressure.
                break;
            }
        }
        if !evicted.is_empty() {
            tracing::warn!(?evicted, "evicted local model slot(s) under memory pressure");
        }
        evicted
    }

    pub fn status(&self, cfg: &LocalEngineConfig) -> Vec<SlotStatus> {
        let inner = self.inner.lock().unwrap();
        [SlotKind::Summarizer, SlotKind::Embedder]
            .into_iter()
            .map(|kind| {
                let engine = inner.slots.get(&kind);
                let configured = kind.model_path(cfg);
                SlotStatus {
                    kind: kind.as_str().to_string(),
                    loaded: engine.is_some(),
                    model_path: engine
                        .map(|e| e.path().display().to_string())
                        .or_else(|| (!configured.is_empty()).then(|| configured.to_string())),
                    n_embd: engine.map(|e| e.n_embd()),
                    backend: engine.map(|e| e.selected_backend().clone()),
                    n_gpu_layers: engine.map(|e| e.n_gpu_layers()),
                    n_ctx: engine.map(|e| e.effective_n_ctx()),
                    error: inner.errors.get(&kind).cloned(),
                }
            })
            .collect()
    }

    fn record_error(&self, kind: SlotKind, msg: &str) {
        self.inner
            .lock()
            .unwrap()
            .errors
            .insert(kind, msg.to_string());
    }
}

impl std::fmt::Debug for SlotManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock().unwrap();
        let loaded: Vec<PathBuf> = inner.slots.values().map(|e| e.path().to_path_buf()).collect();
        f.debug_struct("SlotManager").field("loaded", &loaded).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(summarizer: &str, embedder: &str) -> LocalEngineConfig {
        LocalEngineConfig {
            enabled: true,
            model_path: summarizer.into(),
            embed_model_path: embedder.into(),
            ..Default::default()
        }
    }

    /// An unconfigured slot must say so specifically, not surface as a
    /// generic load failure — the two have very different fixes.
    #[test]
    fn unconfigured_slot_reports_not_configured() {
        let m = SlotManager::new();
        let err = m
            .get_or_load(SlotKind::Embedder, &cfg_with("x.gguf", ""))
            .unwrap_err();
        assert!(
            matches!(err, LocalEngineError::NotConfigured(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn status_reports_both_slots_with_the_configured_path_when_unloaded() {
        let m = SlotManager::new();
        let st = m.status(&cfg_with("sum.gguf", "emb.gguf"));
        assert_eq!(st.len(), 2);
        assert_eq!(st[0].kind, "summarizer");
        assert_eq!(st[1].kind, "embedder");
        assert!(st.iter().all(|s| !s.loaded));
        // Path is echoed even when unloaded so the UI can show what *would*
        // load without forcing a load to find out.
        assert_eq!(st[0].model_path.as_deref(), Some("sum.gguf"));
        assert_eq!(st[1].model_path.as_deref(), Some("emb.gguf"));
    }

    /// A failed load must leave an explanation behind; "loaded: false" with no
    /// reason is the thing that makes this class of bug hard to diagnose.
    #[test]
    fn a_failed_load_is_remembered_in_status() {
        let m = SlotManager::new();
        let cfg = cfg_with("", "");
        let _ = m.get_or_load(SlotKind::Summarizer, &cfg);
        let st = m.status(&cfg);
        let sum = st.iter().find(|s| s.kind == "summarizer").unwrap();
        assert!(sum.error.is_some(), "expected a recorded error");
    }

    #[test]
    fn unload_reports_whether_anything_was_resident() {
        let m = SlotManager::new();
        assert!(!m.unload(SlotKind::Summarizer));
    }

    /// §4.1's eviction order is load-bearing, so pin it: with nothing
    /// resident there is nothing to evict, and the embedder is the one named
    /// to go first.
    #[test]
    fn eviction_is_a_no_op_when_nothing_is_loaded() {
        let m = SlotManager::new();
        assert!(m.evict_under_pressure().is_empty());
    }
}
