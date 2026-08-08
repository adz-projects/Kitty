//! Shared embedding-model convergence: makes sure the pinned Ollama tag the
//! in-process pathway engine depends on is present, pulling it in the
//! background if not. Called at `start_stack` and again periodically by
//! `lifecycle::health::spawn_health_loop` so a model deleted out-of-band, or
//! Ollama coming up late, self-heals without the user touching Settings.

use tauri::{AppHandle, Emitter, Manager};

use super::ollama_proc;
use crate::state::AppState;

/// Readiness of the shared embedding model (`qwen3-embedding:0.6b` by
/// default) that gives the pathway engine real context vectors instead of
/// its lexical-hashing fallback. Never touches `StackStatus` (chat
/// readiness stays independent) — the engine degrades gracefully to hashing
/// embeddings rather than this reading as an outage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingModelStatus {
    /// Not yet checked (e.g. the pathway engine disabled, or Ollama unreachable).
    #[default]
    Unknown,
    /// The pinned tag is installed and ready.
    Present,
    /// A background `ollama pull` is in flight (progress via the existing
    /// `ollama://pull-progress` events, keyed by `pull_id`).
    Downloading,
    /// Checked and not installed; no pull currently running (e.g. Ollama is
    /// down, or the pull attempt failed).
    Missing,
}

/// Payload for the `adaptive_pathway://embedding_status` event — only
/// emitted on change.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AdaptivePathwayEmbeddingStatusPayload {
    pub status: EmbeddingModelStatus,
}

/// Fixed `pull_id` for the embedding-model auto-pull, so a Settings UI can
/// subscribe to `ollama://pull-progress` for this specific pull without
/// having to be told the id first (unlike the wizard's own model pulls,
/// which are user-initiated and get a fresh id each time).
const EMBEDDING_MODEL_PULL_ID: &str = "adaptive-pathway-embedding-model";

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

/// Runtime guarantee (vs. the wizard's best-effort pull): if Ollama is
/// reachable but the pinned embedding-model tag isn't installed, pull it in
/// the background. Non-blocking — `start_stack` continues immediately; the
/// pull's own progress is what drives `EmbeddingModelStatus` to `Present`.
/// Safe to call whenever adaptive-pathway is enabled, including if Ollama
/// itself turns out to be unreachable (reported as `Missing`, not an error).
pub(crate) async fn ensure_embedding_model(app: AppHandle, ollama_base: String, model: String) {
    let client = crate::util::http_client();
    if !ollama_proc::probe_version(&client, &ollama_base).await {
        set_embedding_status(&app, EmbeddingModelStatus::Missing);
        return;
    }
    if ollama_proc::has_model_tag(&client, &ollama_base, &model).await {
        set_embedding_status(&app, EmbeddingModelStatus::Present);
        return;
    }
    set_embedding_status(&app, EmbeddingModelStatus::Downloading);
    tauri::async_runtime::spawn(async move {
        crate::ollama::pull_model(
            app.clone(),
            ollama_base.clone(),
            model.clone(),
            EMBEDDING_MODEL_PULL_ID.to_string(),
        )
        .await;
        // Reuse the same shared client rather than building a second one for
        // the post-pull re-check (Round-7 item 2: this task previously built
        // two clients back-to-back for no reason).
        let client = crate::util::http_client();
        let present = ollama_proc::has_model_tag(&client, &ollama_base, &model).await;
        set_embedding_status(
            &app,
            if present {
                EmbeddingModelStatus::Present
            } else {
                EmbeddingModelStatus::Missing
            },
        );
    });
}
