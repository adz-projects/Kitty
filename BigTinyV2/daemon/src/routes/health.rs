use std::sync::Arc;

use axum::extract::State;
use axum::Extension;
use axum::Json;
use serde_json::{json, Value};

use super::AppState;
use crate::storage::apps::AppIdentity;

/// `GET /api/health` — open, no auth, used by Kitty for readiness polling.
///
/// The `local` block is deliberately coarse (enabled / backend name / how
/// many slots are resident). This route is the one exempt from auth, so it
/// must not leak model paths or device descriptions; `/api/local/models/status`
/// carries the detail.
pub async fn check_health(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        // Both fields exist for discovery, and both are safe to serve
        // unauthenticated: `instance_id` is an opaque per-launch value that
        // grants nothing, and `api_version` is the number a client must know
        // *before* it can decide whether talking to us is safe at all.
        // V1's health route has neither, which is precisely how a client
        // tells a V1 daemon apart from a V2 one.
        "instance_id": state.instance_id,
        "api_version": bigtiny2_protocol::API_VERSION,
        "local": super::local::health_summary(&state),
    }))
}

/// `GET /api/status` — provider health + a coarse daemon status summary.
/// `check_all_health` reuses each provider's cached status within its own
/// 30s TTL rather than probing on literally every call (see
/// `ProviderRouter::check_all_health`'s `HEALTH_TTL_SECS`), and the per-
/// provider status/latency/error it just computed (or reused) is now
/// actually included below rather than discarded in favor of a bare id.
pub async fn status(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Json<Value> {
    state.router.check_all_health().await;
    let providers: Vec<Value> = state
        .router
        .provider_health()
        .into_iter()
        // Only what this app can actually use. A provider id names an
        // endpoint someone configured -- often a hostname, sometimes a
        // Tailscale node -- and its `error` text quotes the endpoint's own
        // response, so an unfiltered list handed every app a directory of
        // everyone else's infrastructure and how well it was working.
        .filter(|(id, _)| state.router.is_visible_to(id, &identity.app_id))
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
