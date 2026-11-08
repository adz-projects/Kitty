//! `/api/apps/me/overnight` — when the calling app's memory engines do their
//! expensive work, and what the last pass did. See `crate::overnight`.
//!
//! Also `/api/apps/me/memorabilia/screen-log`: the paragraphs the intake
//! judged, for labelling and testing other decision-model question sets.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::json;

use crate::overnight::{OvernightConfig, OvernightHub};
use crate::storage::apps::AppIdentity;

use super::AppState;

fn err_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

#[allow(clippy::result_large_err)]
fn hub(state: &AppState) -> Result<&Arc<OvernightHub>, Response> {
    state
        .plugins
        .overnight()
        .ok_or_else(|| err_response(StatusCode::SERVICE_UNAVAILABLE, "overnight learning is not available"))
}

/// `GET /api/apps/me/overnight` — settings (or `null`), the last pass, what
/// is running now, and memorabilia's backlog.
pub async fn get(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    let hub = match hub(&state) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let status = hub.status(&identity.app_id).await;
    let backlog = match state.memorabilia.memorabilia_for(&identity.app_id).await {
        Some(engine) => engine.db.intake_backlog().await.ok(),
        None => None,
    };
    Json(json!({
        "status": status,
        "backlog": backlog,
        "defaults": OvernightConfig::default(),
    }))
    .into_response()
}

/// `PUT /api/apps/me/overnight` — replace the settings.
pub async fn put(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Json(body): Json<OvernightConfig>,
) -> Response {
    let hub = match hub(&state) {
        Ok(h) => h,
        Err(r) => return r,
    };
    match hub.set_config(&identity.app_id, &body).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_response(StatusCode::BAD_REQUEST, e),
    }
}

/// `DELETE /api/apps/me/overnight` — no window: memory work on every sweep.
pub async fn clear(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    let hub = match hub(&state) {
        Ok(h) => h,
        Err(r) => return r,
    };
    match hub.clear(&identity.app_id).await {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// `POST /api/apps/me/overnight/run` — *Process now*: a one-hour window
/// starting at the next sweep tick, which does not wait for the user to be
/// away.
pub async fn run_now(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    let hub = match hub(&state) {
        Ok(h) => h,
        Err(r) => return r,
    };
    if hub.config(&identity.app_id).await.is_none_or(|c| !c.enabled) {
        return err_response(
            StatusCode::CONFLICT,
            "overnight learning is off for this app; memory work already runs as it arrives",
        );
    }
    hub.run_now(&identity.app_id);
    Json(json!({"ok": true})).into_response()
}

#[derive(Debug, Deserialize)]
pub struct ScreenLogQuery {
    #[serde(default)]
    pub limit: Option<i64>,
}

/// Most rows one export returns.
const SCREEN_LOG_MAX: i64 = 5000;

/// `GET /api/apps/me/memorabilia/screen-log?limit=N` — the newest screened
/// paragraphs, newest first.
pub async fn screen_log(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Query(q): Query<ScreenLogQuery>,
) -> Response {
    let Some(engine) = state.memorabilia.memorabilia_for(&identity.app_id).await else {
        return Json(json!({"error": "memorabilia disabled"})).into_response();
    };
    let limit = q.limit.unwrap_or(1000).clamp(1, SCREEN_LOG_MAX);
    match engine.db.list_screen_log(limit).await {
        Ok(rows) => Json(json!({"rows": rows})).into_response(),
        Err(e) => {
            tracing::warn!("screen-log read failed: {e}");
            err_response(StatusCode::INTERNAL_SERVER_ERROR, "could not read the screen log")
        }
    }
}
