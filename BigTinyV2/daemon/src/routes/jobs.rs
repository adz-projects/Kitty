//! `/api/jobs` — work that outlives the client that submitted it.
//!
//! Every V1 route was chat-shaped: create a session, `POST /send`, hold an SSE
//! stream open. That is right for a chat window and wrong for a pipeline —
//! drop the connection and the turn is cancelled after `disconnect_grace_secs`,
//! so a batch job could not survive its submitter restarting.
//!
//! A job is the same turn with the stream taken away: submit, get an id, poll
//! or collect later. The turn machinery is unchanged — `run_turn_and_wait` has
//! always run turns with no live client for recipes and the scheduler; it just
//! had no HTTP surface.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::storage::apps::AppIdentity;
use crate::storage::{jobs, sessions};

use super::AppState;

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

fn job_json(row: &jobs::JobRow) -> Value {
    json!({
        "job_id": row.id,
        "session_id": row.session_id,
        "status": row.status,
        "result": row.result,
        "error": row.error,
        "created_at": row.created_at,
        "started_at": row.started_at,
        "finished_at": row.finished_at,
    })
}

#[derive(Debug, Deserialize)]
pub struct CreateJobRequest {
    pub prompt: String,
    /// Run in an existing session. Omitted, a fresh one is created — which is
    /// the usual case for a pipeline, where each job is independent.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Group this job's session under a parent, for fan-out. Just a tag; the
    /// daemon does no orchestration.
    #[serde(default)]
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

/// `POST /api/jobs`
pub async fn create(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Json(body): Json<CreateJobRequest>,
) -> Response {
    if body.prompt.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "prompt must not be empty");
    }

    // Reuse a session only if this app owns it. Without the check, a job could
    // append a turn to another app's transcript.
    let session_id = match &body.session_id {
        Some(existing) => {
            match sessions::is_owned_by(&state.db, existing, &identity.app_id).await {
                Ok(true) => existing.clone(),
                Ok(false) => {
                    return err(StatusCode::NOT_FOUND, format!("no such session: {existing}"))
                }
                Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            }
        }
        None => {
            let id = uuid::Uuid::new_v4().to_string();
            let name = body.name.clone().unwrap_or_else(|| "job".to_string());
            if let Err(e) =
                sessions::create_session_for_app(&state.db, &id, &name, &identity.app_id).await
            {
                return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
            }
            let mut meta = json!({ "mode": "chat" });
            if let Some(p) = body.provider.as_deref().filter(|p| !p.is_empty()) {
                meta["provider"] = json!(p);
                if let Some(m) = body.model.as_deref().filter(|m| !m.is_empty()) {
                    meta["model"] = json!(m);
                }
            }
            if let Err(e) =
                sessions::update_session_config(&state.db, &id, &meta.to_string()).await
            {
                tracing::warn!("failed to stamp job session metadata: {e}");
            }
            id
        }
    };

    // Fan-out grouping, checked the same way: a parent belonging to another
    // app would let one app graft its subagents onto another's tree.
    if let Some(parent) = body.parent_session_id.as_deref() {
        match sessions::is_owned_by(&state.db, parent, &identity.app_id).await {
            Ok(true) => {
                if let Err(e) = sessions::set_parent(&state.db, &session_id, parent, &identity.app_id).await {
                    tracing::warn!("failed to record parent session: {e}");
                }
            }
            Ok(false) => {
                return err(
                    StatusCode::NOT_FOUND,
                    format!("no such parent session: {parent}"),
                )
            }
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    }

    let job_id = uuid::Uuid::new_v4().to_string();
    if let Err(e) = jobs::create(
        &state.db,
        &job_id,
        &identity.app_id,
        &session_id,
        &body.prompt,
        None,
    )
    .await
    {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    // Detached by construction: the turn runs on its own task and nothing about
    // its progress depends on this response, or on the caller still being here.
    let agent = state.agent.clone();
    let pool = state.db.clone();
    let prompt = body.prompt.clone();
    let sid = session_id.clone();
    let jid = job_id.clone();
    tokio::spawn(async move {
        if let Err(e) = jobs::mark_running(&pool, &jid).await {
            tracing::warn!("could not mark job {jid} running: {e}");
        }
        // Jobs are `Background` in the provider queue: a user waiting on a
        // chat message should not sit behind a batch.
        match agent
            .run_turn_and_wait(&sid, &prompt, crate::provider::queue::Priority::Background)
            .await
        {
            Ok(_notices) => {
                let result = sessions::last_assistant_text(&pool, &sid).await.ok().flatten();
                let _ = jobs::finish(&pool, &jid, "succeeded", result.as_deref(), None).await;
            }
            Err(msg) => {
                let _ = jobs::finish(&pool, &jid, "failed", None, Some(&msg)).await;
            }
        }
    });

    Json(json!({ "job_id": job_id, "session_id": session_id })).into_response()
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `GET /api/jobs`
pub async fn list(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Query(query): Query<ListQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(50);
    match jobs::list_for_app(&state.db, &identity.app_id, query.status.as_deref(), limit).await {
        Ok(rows) => {
            Json(json!({ "jobs": rows.iter().map(job_json).collect::<Vec<_>>() })).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `GET /api/jobs/{id}`
pub async fn get(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(id): Path<String>,
) -> Response {
    match jobs::get_for_app(&state.db, &id, &identity.app_id).await {
        Ok(Some(row)) => Json(job_json(&row)).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, format!("no such job: {id}")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `DELETE /api/jobs/{id}` — cancel.
pub async fn cancel(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(id): Path<String>,
) -> Response {
    // Read first: cancelling the turn needs the session, and the read is also
    // the ownership check.
    let row = match jobs::get_for_app(&state.db, &id, &identity.app_id).await {
        Ok(Some(row)) => row,
        Ok(None) => return err(StatusCode::NOT_FOUND, format!("no such job: {id}")),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    match jobs::cancel_for_app(&state.db, &id, &identity.app_id).await {
        // Already terminal. A 409 rather than a cheerful `ok`, which would
        // tell the caller it stopped work that had in fact finished.
        Ok(0) => err(
            StatusCode::CONFLICT,
            format!("job is already {}", row.status),
        ),
        Ok(_) => {
            if let Some(session_id) = row.session_id.as_deref() {
                state.agent.cancel(session_id).await;
            }
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
