//! `/api/hitl/rules` -- the "always allow" decisions an app's user has made.
//!
//! Rules are recorded from an approval prompt (`POST .../approve` with
//! `always_allow`) and applied to every later tool call from the same app.
//! Until these routes existed there was no way to see them or take one back,
//! so a single click on a risky prompt was permanent. Scoped to the caller:
//! a rule is one app's user's decision and never applies to another app.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::json;

use crate::storage::apps::AppIdentity;
use crate::storage::hitl_rules;

use super::AppState;

fn err_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

/// `GET /api/hitl/rules` -- every rule the caller has recorded, oldest first.
pub async fn list(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    match hitl_rules::list_rules(&state.db, &identity.app_id).await {
        Ok(rules) => Json(rules).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `DELETE /api/hitl/rules/{id}` -- revoke one. Takes effect on the next tool
/// call: rules are read from the database on every check, not cached.
pub async fn delete(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(id): Path<i64>,
) -> Response {
    match hitl_rules::delete_rule(&state.db, &identity.app_id, id).await {
        // Another app's rule reads exactly like a missing one.
        Ok(0) => err_response(StatusCode::NOT_FOUND, format!("no such rule: {id}")),
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `GET /api/apps/me/pending` -- every approval waiting on the calling app,
/// across all its sessions.
///
/// The per-session `/api/chat/{id}/pending` needs the client to know which
/// sessions to ask about, which it cannot for runs it never started (a
/// scheduled task, a job) or after its event stream dropped and came back.
/// Each entry is the same shape as the per-session route's.
pub async fn pending_for_app(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    let pending = {
        let mut hitl = state.agent.hitl().lock().await;
        hitl.sweep_stale_now();
        hitl.all_pending()
    };
    let mut out = Vec::new();
    for action in pending {
        let owner = crate::storage::sessions::owner_of(&state.db, &action.session_id)
            .await
            .ok()
            .flatten();
        if owner.as_deref() == Some(identity.app_id.as_str()) {
            out.push(action.to_dict());
        }
    }
    Json(json!({ "pending": out })).into_response()
}
