//! Public entry points (`ingest`, `recall`, `delete`/`forget`) — grown
//! phase by phase per project-plan.md §15.
//!
//! Phase 0 ships only `recall`, as the soft no-op: with no memory schema or
//! ingestion yet, the payload is byte-identical to no-memory behavior
//! (claude.md, core principle 9). Every entry point added later must keep
//! that property when the engine has nothing to say.
//!
//! Phase 3 adds `forget` (plan §12.2); Phase 4 adds `ingest` (Stages 0–3,
//! plan §3).

use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::config::Config;
use crate::error::Result;
use crate::privacy::PiiClassifier;
use crate::reliability::ReliabilityData;
use crate::store::vectors::SqliteVectorIndex;
use crate::store::Db;
use crate::traits::{Embedder, StructuredChat, VectorIndex};

pub struct Engine {
    /// Single source of truth for every tunable (plan §2).
    pub config: Config,
    /// The engine's own SQLite database (plan §3.3).
    pub db: Db,
    /// Extraction LLM seam; the host implements it over a local provider
    /// (plan §3.6 / §13.1).
    pub chat: Arc<dyn StructuredChat>,
    /// Embedding seam (plan §3.3 / §13.1): semantic-first with lexical
    /// fallback in production; the deterministic hash embedder in tests.
    pub embedder: Arc<dyn Embedder>,
    /// Vector index seam (plan §3.3): brute-force cosine, model-tag
    /// filtered; the `reembed_stale` candidate query is `list_by_model`.
    pub vectors: Arc<dyn VectorIndex>,
    /// Optional LLM PII classifier for the Stage 0 gate (plan §12.1,
    /// flagged `#privacy`); `None` = deterministic patterns only.
    pub pii_classifier: Option<Arc<dyn PiiClassifier>>,
    /// Tier table + reputable-domain data (plan §4.2): loaded automatically
    /// from `data/reliability.yaml`, overridable via
    /// [`Self::with_reliability_data`].
    pub reliability: ReliabilityData,
    /// Global extraction semaphore (plan §2/§3.6: `extraction.max_concurrent`
    /// = 1 — one LLM completion at a time, so concurrent drains never
    /// compete for the local model).
    pub extraction_semaphore: Arc<Semaphore>,
}

impl Engine {
    pub fn new(
        config: Config,
        db: Db,
        chat: Arc<dyn StructuredChat>,
        embedder: Arc<dyn Embedder>,
        vectors: Arc<dyn VectorIndex>,
    ) -> Self {
        let reliability = ReliabilityData::load_default().unwrap_or_else(|e| {
            tracing::warn!(
                "engine: could not load data/reliability.yaml ({e}); using built-in defaults"
            );
            ReliabilityData::default()
        });
        let extraction_semaphore = Arc::new(Semaphore::new(
            config.extraction.max_concurrent.max(1) as usize,
        ));
        Self {
            config,
            db,
            chat,
            embedder,
            vectors,
            pii_classifier: None,
            reliability,
            extraction_semaphore,
        }
    }

    /// Host-agnostic async constructor: open (creating if needed) the SQLite
    /// database at `path`, run migrations, size the `chunks_vec` index to
    /// `config.embedding_dim`, and wire the real [`SqliteVectorIndex`]. The
    /// `chat` and `embedder` seams are injected by the host — the BigTiny
    /// plugin passes adapters over the shared EmbeddingGemma embedder and the
    /// `SummarizerChain` (Gemma E2B → provider fallback); tests use
    /// [`MockChat`](crate::traits::MockChat) + the deterministic hash embedder
    /// with [`Db::open_in_memory`] instead. Returns an `Arc` so one instance
    /// is shared across the recall hook, the learn hook, the background sweep,
    /// and the MCP lookup server (mirrors `PathwayEngine::open_with_embedder`).
    pub async fn open_at(
        path: &str,
        config: Config,
        chat: Arc<dyn StructuredChat>,
        embedder: Arc<dyn Embedder>,
    ) -> Result<Arc<Self>> {
        let db = Db::open(path).await?;
        db.init_vectors(config.embedding_dim).await?;
        let vectors = Arc::new(SqliteVectorIndex::new(db.pool().clone()));
        let engine = Arc::new(Self::new(config, db, chat, embedder, vectors));
        // One-time cleanup of pre-document-harvest dialogue evidence; a no-op
        // (one settings read) once done.
        engine.purge_legacy_conversation_documents().await;
        Ok(engine)
    }

    /// Install the optional LLM PII classifier on the Stage 0 gate (plan
    /// §12.1). Without this, the gate is deterministic patterns only.
    pub fn with_pii_classifier(mut self, classifier: Arc<dyn PiiClassifier>) -> Self {
        self.pii_classifier = Some(classifier);
        self
    }

    /// Override the auto-loaded reliability data file (plan §4.2).
    pub fn with_reliability_data(mut self, data: ReliabilityData) -> Self {
        self.reliability = data;
        self
    }

    /// Set the per-session pause flag (Kitty's unified incognito control —
    /// one chat-header toggle pauses both this engine and adaptive-pathway).
    /// While paused, the daemon loop hooks skip recall injection and turn-end
    /// learn for that session. Persisted, so it survives a daemon restart.
    pub async fn set_paused(&self, session_id: &str, paused: bool) -> Result<()> {
        self.db.set_paused(session_id, paused).await
    }

    /// Whether memorabilia recall/learn are paused for `session_id`.
    pub async fn is_paused(&self, session_id: &str) -> Result<bool> {
        self.db.is_paused(session_id).await
    }
}
