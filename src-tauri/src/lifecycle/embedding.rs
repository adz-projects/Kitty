//! Shared embedding-model convergence: makes sure the pinned Ollama tag
//! Adaptive Pathway depends on is present, pulling it in the background if
//! not. Called at `start_stack` and again periodically by the AP health loop
//! (`lifecycle::health`) so a model deleted out-of-band, or Ollama coming up
//! late, self-heals without the user touching Settings.

use tauri::{AppHandle, Emitter, Manager};

use super::adaptive_pathway_proc::EmbeddingModelStatus;
use super::ollama_proc;
use crate::state::AppState;

/// Payload for the `adaptive_pathway://embedding_status` event — only
/// emitted on change (mirrors `adaptive_pathway://status`).
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
