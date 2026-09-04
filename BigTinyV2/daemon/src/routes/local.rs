//! `GET /api/local/models/status` — local-engine state for Settings.
//!
//! Since the llama.cpp local chat/embedding-slot engine was retired in favour
//! of LiteRT (which has no slot manager or backend picker — it loads one
//! EmbeddingGemma model on a dedicated thread), this route no longer reports
//! resident slots. It is kept so the wire contract stays stable for Kitty's
//! `get_local_engine_status` command and older clients; a 404 would be
//! indistinguishable from a wrong URL. The real LiteRT embedder readiness is
//! observable in the daemon log (`litert embedder ready`) and via memory
//! recall quality, not here.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use super::AppState;

/// GET /api/local/models/status
pub async fn models_status(State(_state): State<Arc<AppState>>) -> Response {
    Json(json!({
        "enabled": false,
        "backend_preference": "cpu",
        "backend_selected": null,
        "devices": [],
        "slots": [],
        "detail": "the llama.cpp local engine was replaced by LiteRT; there is no slot engine to report",
    }))
    .into_response()
}

/// The coarse `local` block `/api/health` embeds. Deliberately thin —
/// `/api/health` is the one route exempt from auth, so it must not leak paths
/// or device details.
pub fn health_summary(_state: &AppState) -> serde_json::Value {
    json!({ "enabled": false, "backend": "none", "slots_loaded": 0 })
}
