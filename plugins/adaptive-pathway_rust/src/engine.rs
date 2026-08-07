//! The `PathwayEngine`: the shared, in-process behavioral-memory engine.
//! Owns the `Pathway`-db pool, the embedding provider, and per-session pause
//! state. Reads are direct in-process calls; writes the model chooses to make
//! go through MCP tools.

use std::sync::Arc;

use dashmap::DashMap;

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
        }))
    }

    pub async fn open_in_memory(cfg: Config) -> Result<Arc<Self>> {
        let db = Db::open_in_memory().await?;
        let embed = EmbeddingProvider::new(cfg.clone());
        Ok(Arc::new(Self {
            db,
            cfg,
            embed,
            paused_override: DashMap::new(),
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
