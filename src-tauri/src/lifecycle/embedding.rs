//! Readiness of the embedding GGUF the in-process pathway engine uses.
//!
//! This used to pull a pinned Ollama tag in the background. With no managed
//! inference process the question collapses to "is the file on disk?" —
//! checked at `start_stack` and again periodically by
//! `lifecycle::health::spawn_health_loop`, so a model deleted out-of-band is
//! noticed without the user touching Settings. Obtaining one is now an
//! explicit action in Settings → Local Models rather than something that
//! happens behind the user's back.

use tauri::{AppHandle, Emitter, Manager};

use crate::state::AppState;

/// Readiness of the shared embedding model that gives the pathway engine real
/// context vectors instead of its lexical-hashing fallback. Never touches
/// `StackStatus` (chat readiness stays independent) — the engine degrades
/// gracefully to hashing embeddings rather than this reading as an outage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingModelStatus {
    /// Not yet checked (e.g. the pathway engine is disabled).
    #[default]
    Unknown,
    /// The GGUF is present and ready.
    Present,
    /// A download is in flight (progress via `models://progress`, keyed by
    /// `download_id`).
    Downloading,
    /// Checked and not on disk; no download currently running.
    Missing,
}

/// Payload for the `adaptive_pathway://embedding_status` event — only
/// emitted on change.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AdaptivePathwayEmbeddingStatusPayload {
    pub status: EmbeddingModelStatus,
}

pub(crate) fn set_embedding_status(app: &AppHandle, status: EmbeddingModelStatus) {
    let changed = {
        let state = app.state::<AppState>();
        let mut cur = state.adaptive_pathway_embedding_status.lock().unwrap();
        if *cur != status {
            *cur = status;
            true
        } else {
            false
        }
    };
    if changed {
        let _ = app.emit(
            "adaptive_pathway://embedding_status",
            AdaptivePathwayEmbeddingStatusPayload { status },
        );
    }
}

/// Re-derive `Present`/`Missing` from what's actually on disk.
///
/// Cheap and synchronous — a directory listing — so it's safe on the health
/// loop's 30s cadence. A missing model is reported, never auto-fixed: pulling
/// hundreds of megabytes without asking is exactly the behaviour retiring
/// managed Ollama was meant to end.
pub(crate) fn refresh_embedding_status(app: &AppHandle, model: &str) {
    let present = crate::models::resolve(model).is_some();
    set_embedding_status(
        app,
        if present {
            EmbeddingModelStatus::Present
        } else {
            EmbeddingModelStatus::Missing
        },
    );
}
