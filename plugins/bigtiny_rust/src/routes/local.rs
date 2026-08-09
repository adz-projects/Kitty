//! `GET /api/local/models/status` — resident local-model state (§3.1, §6.3).
//!
//! Backs the Settings model card: which slots are loaded, from which file, at
//! what embedding width, on which compute backend, and why a slot is empty
//! when it is.
//!
//! **`reload_required`/`restart_pending` are deliberately not here**, despite
//! §3.1 listing them. The daemon cannot know either: it receives its config
//! as env vars at spawn and has no channel to be told a setting changed, and
//! it is Kitty that owns the restart (`commands::restart_backend`) and tracks
//! in-flight turns (`AppState::in_flight_sessions`). Reporting those two from
//! here would mean the daemon guessing at a decision made in another process.
//! They live on the Kitty side instead — see `lifecycle::engine_restart`.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use super::AppState;

/// GET /api/local/models/status
#[cfg(feature = "local-engine")]
pub async fn models_status(State(state): State<Arc<AppState>>) -> Response {
    let cfg = &state.config.local;
    let slots = state.local_slots.status(cfg);
    // What a load *would* pick right now, which can differ from what a
    // resident model is on (the config changed but nothing reloaded yet).
    // Both are reported so the card can say so rather than silently showing
    // one and meaning the other.
    let selected = crate::local::select_backend(&cfg.backend);
    Json(json!({
        "enabled": cfg.enabled,
        "backend_preference": cfg.backend,
        "backend_selected": selected,
        "devices": crate::local::backend::available_devices(),
        "slots": slots,
    }))
    .into_response()
}

/// Without the feature the route still exists so the wire contract is stable
/// across builds — a 404 would be indistinguishable from a wrong URL. Same
/// reasoning as `routes::embeddings`.
#[cfg(not(feature = "local-engine"))]
pub async fn models_status(State(_state): State<Arc<AppState>>) -> Response {
    Json(json!({
        "enabled": false,
        "backend_preference": "cpu",
        "backend_selected": null,
        "devices": [],
        "slots": [],
        "detail": "this build has no local engine (feature `local-engine` is off)",
    }))
    .into_response()
}

/// The coarse `local` block `/api/health` embeds.
///
/// Kept deliberately thin: `/api/health` is the one route exempt from auth
/// (readiness polling needs no key), so it must not leak filesystem paths or
/// device descriptions. Anything richer belongs on the authed status route
/// above.
#[cfg(feature = "local-engine")]
pub fn health_summary(state: &AppState) -> serde_json::Value {
    let cfg = &state.config.local;
    let slots = state.local_slots.status(cfg);
    json!({
        "enabled": cfg.enabled,
        "backend": crate::local::select_backend(&cfg.backend).backend,
        "slots_loaded": slots.iter().filter(|s| s.loaded).count(),
    })
}

#[cfg(not(feature = "local-engine"))]
pub fn health_summary(_state: &AppState) -> serde_json::Value {
    json!({ "enabled": false, "backend": "none", "slots_loaded": 0 })
}
