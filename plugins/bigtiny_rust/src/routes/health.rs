use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use super::AppState;

/// `GET /api/health` — open, no auth, used by Kitty for readiness polling.
///
/// The `local` block is deliberately coarse (enabled / backend name / how
/// many slots are resident). This route is the one exempt from auth, so it
/// must not leak model paths or device descriptions; `/api/local/models/status`
/// carries the detail.
pub async fn check_health(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "local": super::local::health_summary(&state),
    }))
}

/// `GET /api/status` — provider health + a coarse daemon status summary.
/// `check_all_health` reuses each provider's cached status within its own
/// 30s TTL rather than probing on literally every call (see
/// `ProviderRouter::check_all_health`'s `HEALTH_TTL_SECS`), and the per-
/// provider status/latency/error it just computed (or reused) is now
/// actually included below rather than discarded in favor of a bare id.
pub async fn status(State(state): State<Arc<AppState>>) -> Json<Value> {
    state.router.check_all_health().await;
    let providers: Vec<Value> = state
        .router
        .provider_health()
        .into_iter()
        .map(|(id, health)| {
            json!({
                "id": id,
                "status": health.status,
                "latency_ms": health.latency_ms,
                "error": health.error,
            })
        })
        .collect();
    Json(json!({"status": "ok", "providers": providers}))
}
