//! `/api/apps/me/approval-policy` — the calling app's automatic approval
//! policy (see `agent::approval_policy`). Per app: one app's policy never
//! approves another app's calls.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::json;

use crate::agent::approval_policy::ApprovalPolicyConfig;
use crate::storage::apps::AppIdentity;

use super::AppState;

/// `GET` → `{policy | null}`.
pub async fn get(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    let policy = state
        .agent
        .approval_policies()
        .for_app(&identity.app_id)
        .await
        .map(|p| p.config.clone());
    Json(json!({ "policy": policy })).into_response()
}

/// `PUT` `{auto_allow, shell_sensitive_pattern?}` replaces it (400 on a
/// pattern that does not compile).
pub async fn put(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Json(body): Json<ApprovalPolicyConfig>,
) -> Response {
    match state
        .agent
        .approval_policies()
        .set(&identity.app_id, Some(body))
        .await
    {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

/// `DELETE` removes it: every call pauses again.
pub async fn clear(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    match state.agent.approval_policies().set(&identity.app_id, None).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}
