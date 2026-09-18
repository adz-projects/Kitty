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
    /// The engine `Config`, pre-tuned to the shared embedder's vector space
    /// (dim + tag) so every app's vectors are comparable.
    mem_config: MemConfig,
    /// Built once from the shared embedder (or the hash fallback) and cloned
    /// into each engine — never a second loaded model.
    embedder: Arc<dyn memorabilia::traits::Embedder>,
    /// The extraction/maintenance chat seam (SummarizerChain), daemon-wide.
    chat: Arc<dyn memorabilia::traits::StructuredChat>,
    instances: Mutex<HashMap<String, Instance>>,
}

impl MemorabiliaHost {
    /// Build the host. `embedder` is the daemon's shared semantic embedder
    /// (`None` → memorabilia's lexical hash embedder), `embed_dim` its live
    /// vector width, `embed_space` its vector-space identity tag, `chat` the
    /// `SummarizerChain`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: SqlitePool,
        data_dir: PathBuf,
        default_enabled: bool,
        db_name: String,
        sweep_interval_s: u64,
        embedder: Option<Arc<dyn SemanticEmbedder>>,
        embed_dim: usize,
        embed_space: String,
        chat: Arc<SummarizerChain>,
    ) -> Self {
        let mut mem_config = MemConfig::default();
        mem_config.embedding_dim = embed_dim;
        // Tag the vector space with the shared model's identity when a
        // semantic embedder is present; otherwise keep the hash-fallback tag
        // discipline (the adapter returns HASH_EMBED_MODEL per call anyway).
        if embedder.is_some() {
            mem_config.embedding_model = embed_space.clone();
        }
        mem_config.maintenance_tick_s = sweep_interval_s;

        let embedder: Arc<dyn memorabilia::traits::Embedder> = match embedder {
            Some(inner) => Arc::new(SharedSemanticEmbedder {
                inner,
                dim: embed_dim,
                tag: embed_space,
            }),
            None => Arc::new(memorabilia::embed::HashEmbedder::new(embed_dim)),
        };
        let chat: Arc<dyn memorabilia::traits::StructuredChat> =
            Arc::new(SummarizerChatAdapter { inner: chat });

        Self {
            pool,
            data_dir,
            default_enabled,
            db_name,
            sweep_interval_s,
            mem_config,
            embedder,
            chat,
            instances: Mutex::new(HashMap::new()),
        }
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

        let engine = match Engine::open_at(
            &db_path.to_string_lossy(),
            self.mem_config.clone(),
            self.chat.clone(),
            self.embedder.clone(),
        )
        .await
        {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!("memorabilia engine failed to open at {db_path:?}: {err}");
                return None;
            }
        };

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
