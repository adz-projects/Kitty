//! Ensures BigTiny's summarizer model (`Config::summarizer.model`, e.g.
//! `qwen3.5:0.8b`) is present in Ollama, pulling it in the background if not.
//! Called at `start_stack` and again periodically by `health::spawn_health_loop`
//! so a model deleted out-of-band, or Ollama coming up late, self-heals
//! without the user touching Settings — same rationale and shape as
//! `embedding::ensure_embedding_model` for adaptive-pathway's embedding
//! model, just without a dedicated status enum/event since nothing in the UI
//! currently needs to observe this beyond the shared `ollama://pull-progress`
//! stream.

use std::sync::atomic::Ordering;

use tauri::AppHandle;

use super::ollama_proc;
use crate::state::AppState;
use tauri::Manager;

/// Fixed `pull_id` for the summarizer-model auto-pull (mirrors
/// `embedding::EMBEDDING_MODEL_PULL_ID`) so a future Settings UI could
/// subscribe to `ollama://pull-progress` for this specific pull without
/// needing to be told the id first.
const SUMMARIZER_MODEL_PULL_ID: &str = "bigtiny-summarizer-model";

/// Non-blocking: returns quickly whether or not a pull is needed. If the
/// model is missing, kicks off a background pull and returns immediately —
/// BigTiny itself doesn't need the model to be present to *start*, only to
/// actually run a summarization pass, so this never delays `start_stack`.
/// Guarded by `AppState::summarizer_model_pulling` so the startup call and
/// the periodic health-loop re-check can never race into pulling the same
/// tag twice concurrently.
pub(crate) async fn ensure_summarizer_model(app: AppHandle, ollama_base: String, model: String) {
    let client = crate::util::http_client();
    if !ollama_proc::probe_version(&client, &ollama_base).await {
        return;
    }
    if ollama_proc::has_model_tag(&client, &ollama_base, &model).await {
        return;
    }

    let state = app.state::<AppState>();
    if state
        .summarizer_model_pulling
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // Already pulling (either the startup call or a previous health-loop
        // tick beat us to it) — nothing more to do here.
        return;
    }

    tauri::async_runtime::spawn(async move {
        crate::ollama::pull_model(
            app.clone(),
            ollama_base,
            model,
            SUMMARIZER_MODEL_PULL_ID.to_string(),
        )
        .await;
        app.state::<AppState>()
            .summarizer_model_pulling
            .store(false, Ordering::Release);
    });
}
