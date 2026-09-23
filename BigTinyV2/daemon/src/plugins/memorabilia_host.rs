//! Per-app memorabilia (declarative factual memory) instances, opened lazily
//! and shut down when idle. Mirrors [`super::host::PluginHost`] exactly, but
//! for `memorabilia::engine::Engine` instead of `PathwayEngine`.
//!
//! The two seams the engine depends on are supplied here as thin adapters over
//! resources the daemon already owns: [`SharedSemanticEmbedder`] wraps the
//! shared LiteRT EmbeddingGemma model (the same `Arc<dyn SemanticEmbedder>`
//! pathway uses — never a second loaded model), and [`SummarizerChatAdapter`]
//! wraps the `SummarizerChain` (Gemma E2B → provider fallback) so extraction
//! and maintenance reasoning run through the daemon's existing local-first
//! chat. When no embedder is configured the engine falls back to memorabilia's
//! own deterministic lexical hash embedder.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use adaptive_pathway::embed::SemanticEmbedder;
use adaptive_pathway::traits::StructuredChat as ApStructuredChat;
use async_trait::async_trait;
use memorabilia::config::{Config as MemConfig, HASH_EMBED_MODEL};
use memorabilia::engine::Engine;
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::sync::Mutex;

use crate::agent::summarizer_chain::SummarizerChain;
use crate::storage::app_plugins::{self, MEMORABILIA};

/// Adapts the daemon's shared `SemanticEmbedder` (native-width vectors, `None`
/// on failure) to memorabilia's `Embedder` seam (returns the vector plus the
/// tag of the space that produced it, with a deterministic lexical fallback).
struct SharedSemanticEmbedder {
    inner: Arc<dyn SemanticEmbedder>,
    dim: usize,
    tag: String,
}

#[async_trait]
impl memorabilia::traits::Embedder for SharedSemanticEmbedder {
    async fn embed(&self, text: &str) -> Result<(Vec<f32>, String), String> {
        match self.inner.embed(text).await {
            Some(v) => {
                // The daemon sizes `dim` from the live model, so this is
                // normally identity; project only if a Matryoshka-truncated
                // config asks for a narrower width.
                let v = if v.len() == self.dim {
                    v
                } else {
                    memorabilia::embed::project(&v, self.dim)
                };
                Ok((v, self.tag.clone()))
            }
            None => Ok((
                memorabilia::embed::hash_embed(text, self.dim),
                HASH_EMBED_MODEL.to_string(),
            )),
        }
    }

    fn model_tag(&self) -> &str {
        &self.tag
    }
}

/// Adapts the daemon's `SummarizerChain` (which implements
/// `adaptive_pathway::traits::StructuredChat`) to memorabilia's identical
/// `StructuredChat` seam — the Gemma E2B → provider fallback extraction path.
struct SummarizerChatAdapter {
    inner: Arc<SummarizerChain>,
}

#[async_trait]
impl memorabilia::traits::StructuredChat for SummarizerChatAdapter {
    async fn structured_chat(
        &self,
        messages: Vec<Value>,
        schema: &Value,
    ) -> Result<Value, String> {
        ApStructuredChat::structured_chat(self.inner.as_ref(), messages, schema).await
    }
}

/// One app's live memorabilia instance.
struct Instance {
    engine: Arc<Engine>,
    shutdown: tokio::sync::watch::Sender<bool>,
    background: tokio::task::AbortHandle,
}

/// Hosts the memorabilia plugin, one engine per app (parallel to `PluginHost`).
pub struct MemorabiliaHost {
    pool: SqlitePool,
    data_dir: PathBuf,
    default_enabled: bool,
    db_name: String,
    sweep_interval_s: u64,
    /// The shared semantic embedder (the same one pathway uses; `None` → the
    /// engine's own lexical hash fallback). Its live vector width is probed
    /// **lazily** on the first engine open, not at construction: a cold LiteRt
    /// embed on the daemon startup path used to block `/api/health` until the
    /// model finished loading (a "stack degraded" flash on first launch).
    raw_embedder: Option<Arc<dyn SemanticEmbedder>>,
    /// The shared model's vector-space identity tag.
    embed_space: String,
    /// The extraction/maintenance chat seam (SummarizerChain), daemon-wide.
    chat: Arc<dyn memorabilia::traits::StructuredChat>,
    /// Memoized `(config, embedder)` resolved from `raw_embedder` on first use.
    /// Sized to the embedder's live width once, then fixed for the process so
    /// every app's `chunks_vec` table agrees on a dimension.
    resolved: tokio::sync::OnceCell<(MemConfig, Arc<dyn memorabilia::traits::Embedder>)>,
    instances: Mutex<HashMap<String, Instance>>,
    /// Last engine-open failure per app, cleared on a successful open. Lets the
    /// routes say *why* memory is unavailable instead of reporting every failed
    /// open as "disabled" (which hid the 0.11.2 migration-checksum failure).
    open_errors: Mutex<HashMap<String, String>>,
    /// Apps whose DB file has been integrity-checked (and rebuilt if corrupt)
    /// at first open in this process — see `db_recover::heal_before_open`. The
    /// value is the rebuild report, held until `recover` reports it once.
    healed: Mutex<HashMap<String, Option<crate::plugins::db_recover::RecoverReport>>>,
}

impl MemorabiliaHost {
    /// Build the host. `embedder` is the daemon's shared semantic embedder
    /// (`None` → memorabilia's lexical hash embedder), `embed_space` its
    /// vector-space identity tag, `chat` the `SummarizerChain`. The embedder's
    /// live width is **not** probed here — see [`Self::resolved`] — so
    /// construction is cheap and cannot block daemon startup.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: SqlitePool,
        data_dir: PathBuf,
        default_enabled: bool,
        db_name: String,
        sweep_interval_s: u64,
        embedder: Option<Arc<dyn SemanticEmbedder>>,
        embed_space: String,
        chat: Arc<SummarizerChain>,
    ) -> Self {
        let chat: Arc<dyn memorabilia::traits::StructuredChat> =
            Arc::new(SummarizerChatAdapter { inner: chat });

        Self {
            pool,
            data_dir,
            default_enabled,
            db_name,
            sweep_interval_s,
            raw_embedder: embedder,
            embed_space,
            chat,
            resolved: tokio::sync::OnceCell::new(),
            instances: Mutex::new(HashMap::new()),
            open_errors: Mutex::new(HashMap::new()),
            healed: Mutex::new(HashMap::new()),
        }
    }

    /// Why this app's engine last failed to open, if it did (and hasn't opened
    /// successfully since).
    pub async fn open_error(&self, app_id: &str) -> Option<String> {
        self.open_errors.lock().await.get(app_id).cloned()
    }

    /// Resolve `(engine config, embedder)` once, lazily, memoized. The shared
    /// embedder's live vector width is probed on the first engine open (during
    /// a turn, when the model is warm) rather than at daemon startup, so a cold
    /// LiteRt load never delays `/api/health`. The probe is time-bounded; on
    /// timeout or failure it falls back to memorabilia's own lexical hash
    /// embedder at the default width. The result is fixed for the process, so
    /// every app's `chunks_vec` table is created with the same dimension.
    async fn resolved(&self) -> &(MemConfig, Arc<dyn memorabilia::traits::Embedder>) {
        self.resolved
            .get_or_init(|| async {
                let default_dim = MemConfig::default().embedding_dim;
                let (dim, embedder, semantic): (
                    usize,
                    Arc<dyn memorabilia::traits::Embedder>,
                    bool,
                ) = match &self.raw_embedder {
                    Some(e) => match tokio::time::timeout(
                        Duration::from_secs(30),
                        e.embed("dimension probe"),
                    )
                    .await
                    {
                        Ok(Some(v)) if !v.is_empty() => {
                            let dim = v.len();
                            (
                                dim,
                                Arc::new(SharedSemanticEmbedder {
                                    inner: e.clone(),
                                    dim,
                                    tag: self.embed_space.clone(),
                                }),
                                true,
                            )
                        }
                        _ => {
                            tracing::warn!(
                                "memorabilia: embedder width probe failed/timed out; \
                                 using the lexical hash fallback"
                            );
                            (
                                default_dim,
                                Arc::new(memorabilia::embed::HashEmbedder::new(default_dim)),
                                false,
                            )
                        }
                    },
                    None => (
                        default_dim,
                        Arc::new(memorabilia::embed::HashEmbedder::new(default_dim)),
                        false,
                    ),
                };
                let mut mem_config = MemConfig::default();
                mem_config.embedding_dim = dim;
                mem_config.maintenance_tick_s = self.sweep_interval_s;
                // Tag the vector space with the shared model's identity only
                // when the semantic embedder actually resolved; the hash
                // fallback keeps its own per-call HASH_EMBED_MODEL tag.
                if semantic {
                    mem_config.embedding_model = self.embed_space.clone();
                }
                (mem_config, embedder)
            })
            .await
    }

    fn db_path(&self, app_id: &str) -> PathBuf {
        self.data_dir.join("apps").join(app_id).join(&self.db_name)
    }

    /// Whether memorabilia is on for this app: its own preference, else the
    /// daemon default.
    pub async fn is_enabled(&self, app_id: &str) -> bool {
        match app_plugins::is_enabled(&self.pool, app_id, MEMORABILIA).await {
            Ok(Some(explicit)) => explicit,
            Ok(None) => self.default_enabled,
            Err(e) => {
                tracing::warn!("could not read memorabilia preference for {app_id}: {e}");
                self.default_enabled
            }
        }
    }

    /// This app's engine, opening one if needed. `None` when memorabilia is
    /// off for the app, or when the engine could not be opened.
    pub async fn memorabilia_for(&self, app_id: &str) -> Option<Arc<Engine>> {
        if let Some(existing) = self.instances.lock().await.get(app_id) {
            return Some(existing.engine.clone());
        }
        if !self.is_enabled(app_id).await {
            return None;
        }

        // Resolve the shared config/embedder before taking the write lock, so
        // the one-time (bounded) width probe never holds it. Memoized, so this
        // is instant on every open after the first.
        let (mem_config, embedder) = {
            let r = self.resolved().await;
            (r.0.clone(), r.1.clone())
        };

        let mut instances = self.instances.lock().await;
        if let Some(existing) = instances.get(app_id) {
            return Some(existing.engine.clone());
        }

        let db_path = self.db_path(app_id);
        if let Some(parent) = db_path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                tracing::warn!("could not create memorabilia dir {parent:?}: {e}");
                return None;
            }
        }

        // First open of this app's engine in this process: nothing else holds
        // the file yet, so this is the one safe moment to rebuild it if it is
        // corrupt (see `db_recover::heal_before_open`). A rebuild recreates the
        // `chunks_vec` index empty (vec tables aren't copyable rows), so
        // salvaged chunks keep their text but need re-embedding to be found by
        // vector search.
        {
            let mut healed = self.healed.lock().await;
            if !healed.contains_key(app_id) {
                let report = crate::plugins::db_recover::heal_before_open(
                    "memorabilia",
                    &db_path,
                    |p| async move {
                        memorabilia::store::Db::open(&p.to_string_lossy())
                            .await
                            .map(|db| db.pool().clone())
                            .map_err(|e| e.to_string())
                    },
                )
                .await;
                healed.insert(app_id.to_string(), report);
            }
        }

        let engine = match Engine::open_at(
            &db_path.to_string_lossy(),
            mem_config,
            self.chat.clone(),
            embedder,
        )
        .await
        {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!("memorabilia engine failed to open at {db_path:?}: {err}");
                self.open_errors
                    .lock()
                    .await
                    .insert(app_id.to_string(), err.to_string());
                return None;
            }
        };
        self.open_errors.lock().await.remove(app_id);

        let (shutdown, rx) = tokio::sync::watch::channel(false);
        let background =
            tokio::spawn(sweep(engine.clone(), self.sweep_interval_s, rx)).abort_handle();

        tracing::info!(app_id, path = ?db_path, "opened memorabilia instance");
        instances.insert(
            app_id.to_string(),
            Instance {
                engine: engine.clone(),
                shutdown,
                background,
            },
        );
        Some(engine)
    }

    /// Close one app's instance and stop its background sweep.
    pub async fn close(&self, app_id: &str) {
        let Some(instance) = self.instances.lock().await.remove(app_id) else {
            return;
        };
        let _ = instance.shutdown.send(true);
        instance.background.abort();
        tracing::info!(app_id, "closed memorabilia instance");
    }

    /// Close every instance. Called on daemon shutdown.
    pub async fn shutdown(&self) {
        let mut instances = self.instances.lock().await;
        for (app_id, instance) in instances.drain() {
            let _ = instance.shutdown.send(true);
            instance.background.abort();
            tracing::debug!(app_id, "closed memorabilia instance on shutdown");
        }
    }

    /// Settings → Memorabilia Health "Check & repair database". Never closes or
    /// swaps a live engine's file (the in-process MCP server keeps its own
    /// `Arc` to the engine, so a close/reopen here would leave two connections
    /// writing the same file). Instead:
    /// - makes sure the engine is open (which heals a corrupt file on the first
    ///   open of this process) and reports any rebuild that did, once;
    /// - integrity-checks the file on a separate connection, and if it's
    ///   corrupt now, sets `restart_required` — the next process heals it;
    /// - reports `open_error` if memorabilia is on but the engine won't start.
    pub async fn recover(&self, app_id: &str) -> crate::plugins::db_recover::RecoverReport {
        use crate::plugins::db_recover as rec;
        let db_path = self.db_path(app_id);

        let opened = self.memorabilia_for(app_id).await.is_some();
        let mut report = self
            .healed
            .lock()
            .await
            .get_mut(app_id)
            .and_then(Option::take)
            .unwrap_or_default();

        match rec::check_file(&db_path).await {
            None | Some(true) => report.integrity_ok = true,
            Some(false) => {
                report.integrity_ok = false;
                report.restart_required = true;
            }
        }
        if !opened && self.is_enabled(app_id).await {
            report.open_error = Some(
                self.open_error(app_id)
                    .await
                    .unwrap_or_else(|| "the memory engine could not be started".into()),
            );
        }
        report
    }

    /// How many instances are currently open (diagnostics/tests).
    pub async fn open_instances(&self) -> usize {
        self.instances.lock().await.len()
    }
}

/// The per-instance background maintenance sweep: one bounded
/// `maintenance_tick` on the configured cadence until the instance is closed.
/// The engine owns time via the tick's `now` argument; the wall clock is fine
/// here because the tick is a cadence, not decay math (which is chunk-anchored).
async fn sweep(engine: Arc<Engine>, interval_s: u64, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(interval_s.max(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
                engine.maintenance_tick(&now).await;
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
        }
    }
}
