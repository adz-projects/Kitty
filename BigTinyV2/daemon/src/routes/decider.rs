//! `/api/apps/me/decider` — the calling app's decision model.
//!
//! Per app because the engine is shared: one app's decision model never
//! steers another app's turns. See `crate::decider`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::decider::{DeciderConfig, DeciderHub, Thresholds, DIALECTS};
use crate::storage::apps::AppIdentity;

use super::AppState;

fn err_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

/// `GET /api/apps/me/decider` — the stored configuration (or `null`), plus
/// what a settings UI needs to edit it: the dialects this engine speaks and
/// the default thresholds.
pub async fn get(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    let config = state
        .agent
        .decider()
        .for_app(&identity.app_id)
        .await
        .map(|d| d.config.clone());
    Json(json!({
        "config": config,
        "dialects": DIALECTS,
        "default_thresholds": Thresholds::default(),
    }))
    .into_response()
}

/// `PUT /api/apps/me/decider` — replace the configuration.
pub async fn put(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Json(body): Json<DeciderConfig>,
) -> Response {
    match state
        .agent
        .decider()
        .set(&identity.app_id, Some(body))
        .await
    {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_response(StatusCode::BAD_REQUEST, e),
    }
}

/// `DELETE /api/apps/me/decider` — no decision model for this app.
pub async fn clear(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    match state.agent.decider().set(&identity.app_id, None).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// `GET /api/apps/me/decider/stats` — per-feature counters since the engine
/// started: `calls`, `answered`, `failed` (fell back) and `acted` (an answer
/// changed what the engine did).
pub async fn stats(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    let mut features = Map::new();
    for (f, [calls, answered, failed, acted]) in state.agent.decider().snapshot(&identity.app_id) {
        features.insert(
            f.as_str().to_string(),
            json!({"calls": calls, "answered": answered, "failed": failed, "acted": acted}),
        );
    }
    Json(json!({ "features": Value::Object(features) })).into_response()
}

#[derive(Debug, Deserialize)]
pub struct ProbeRequest {
    #[serde(default)]
    pub dialect: Option<String>,
    pub url: String,
}

/// `POST /api/apps/me/decider/probe` — test an endpoint without saving it:
/// `{ok, model, rtt_ms, error}`.
pub async fn probe(Json(body): Json<ProbeRequest>) -> Response {
    let dialect = body
        .dialect
        .unwrap_or_else(|| crate::decider::systemone::DIALECT.to_string());
    Json(DeciderHub::probe(&dialect, &body.url).await).into_response()
}

#[derive(Debug, Deserialize)]
pub struct ScreenRequest {
    pub command: String,
}

/// `POST /api/apps/me/decider/screen-command` `{command}` → `{risk, flagged}`,
/// or `{risk: null, flagged: false}` when the shell screen is off or had no
/// answer. Escalation only: an app should hold a flagged command for a
/// person, and treat anything else exactly as it would have.
pub async fn screen_command(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Json(body): Json<ScreenRequest>,
) -> Response {
    match crate::decider::shell_screen::screen(state.agent.decider(), &identity.app_id, &body.command)
        .await
    {
        Some(v) => Json(json!({"risk": v.risk, "flagged": v.flagged})).into_response(),
        None => Json(json!({"risk": null, "flagged": false})).into_response(),
    }
}
