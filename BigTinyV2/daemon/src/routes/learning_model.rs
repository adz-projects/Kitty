//! `/api/apps/me/learning-model` — which model the calling app's memory
//! learning runs on. See `crate::agent::learning_model`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::json;

use crate::agent::learning_model::{self, LearningModel};
use crate::storage::apps::AppIdentity;

use super::AppState;

/// `GET` → `{learning_model | null}` (`null` = `auto`).
pub async fn get(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    match learning_model::get_stored(&state.db, &identity.app_id).await {
        Ok(choice) => Json(json!({ "learning_model": choice })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

/// `PUT` `{kind: "auto" | "chat" | "provider", provider_id?, model?}`.
pub async fn put(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Json(body): Json<LearningModel>,
) -> Response {
    match learning_model::set(&state.db, &identity.app_id, Some(&body)).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

/// `DELETE` → back to `auto`.
pub async fn clear(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    match learning_model::set(&state.db, &identity.app_id, None).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}
