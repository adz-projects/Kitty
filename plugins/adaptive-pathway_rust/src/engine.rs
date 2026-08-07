//! The `PathwayEngine`: the shared, in-process behavioral-memory engine.
//! Owns the `Pathway`-db pool, the embedding provider, and per-session pause
//! state. Reads are direct in-process calls; writes the model chooses to make
//! go through MCP tools.

use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::embed::provider::EmbeddingProvider;
use crate::error::Result;
use crate::store::Db;

pub struct PathwayEngine {
    pub db: Db,
    pub cfg: Config,
    pub embed: EmbeddingProvider,
    /// Mirrors `conversation_state.paused` for the hot path. `None` (absent)
    /// means configured/available; a paused session maps to `Some(true)`.
    paused_override: DashMap<String, bool>,
    /// Per-session learn lock, held for the duration of one learn pass.
    learn_locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// Global 1-permit semaphore around every `structured_chat`.
    chat_slot: Arc<Semaphore>,
}

impl PathwayEngine {
    pub async fn open(path: &str, cfg: Config) -> Result<Arc<Self>> {
        let db = Db::open(path).await?;
        let embed = EmbeddingProvider::new(cfg.clone());
        Ok(Arc::new(Self {
            db,
            cfg,
            embed,
            paused_override: DashMap::new(),
            learn_locks: DashMap::new(),
            chat_slot: Arc::new(Semaphore::new(1)),
        }))
    }

    /// A per-session learn lock, released when the returned guard drops.
    pub async fn learn_lock(&self, session_id: &str) -> Result<tokio::sync::OwnedMutexGuard<()>> {
        let guard = self
            .learn_locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())));
        let lock = guard.clone();
        Ok(lock.lock_owned().await)
    }

    /// Whether learning should proceed for this session (reads the pause
    /// override map; the DB is authoritative but this is the hot-path check).
    pub async fn learn_paused(&self, session_id: &str) -> Option<bool> {
        self.paused_override.get(session_id).map(|p| *p)
    }

    /// Acquire the global chat permit (one structured_chat at a time).
    /// Handle to the global semaphore, for callers that want the permit
    /// helper.
    pub fn chat_semaphore(&self) -> Arc<Semaphore> {
        self.chat_slot.clone()
    }

    pub async fn open_in_memory(cfg: Config) -> Result<Arc<Self>> {
        let db = Db::open_in_memory().await?;
        let embed = EmbeddingProvider::new(cfg.clone());
        Ok(Arc::new(Self {
            db,
            cfg,
            embed,
            paused_override: DashMap::new(),
            learn_locks: DashMap::new(),
            chat_slot: Arc::new(Semaphore::new(1)),
        }))
    }

    /// Is recall paused for `session_id`? Mirrors the DB (`conversation_state`)
    /// plus any in-memory override.
    pub async fn is_paused(&self, session_id: &str) -> Result<bool> {
        if let Some(p) = self.paused_override.get(session_id) {
            return Ok(*p);
        }
        self.db.is_paused(session_id).await
    }

    /// Set the per-session pause flag (incognito). Persisted to
    /// `conversation_state.paused` and mirrored to the DashMap for the hot
    /// path.
    pub async fn set_paused(&self, session_id: &str, paused: bool) -> Result<()> {
        self.db.set_paused(session_id, paused).await?;
        self.paused_override.insert(session_id.to_string(), paused);
        Ok(())
    }
}
