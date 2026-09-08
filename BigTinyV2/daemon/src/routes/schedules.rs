//! `/api/schedules` routes — mirrors
//! `plugins/bigtiny/bigtiny/server/routes/schedules.py`. `create_schedule`
//! and `run_now` go through the live `Scheduler` (not just `storage::schedules`
//! directly) so a newly-created job is registered immediately rather than
//! only taking effect after the next daemon restart.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::{json, Value};

use crate::error::SchedulerError;
use crate::storage::apps::AppIdentity;
use crate::storage::schedules;

use super::AppState;

fn err_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

/// Map a `SchedulerError` to the right HTTP status — a genuinely-missing
/// schedule is a 404; a cron-validation failure is the caller's bad input
/// (400, not a 500 that reads as "daemon broken"); storage failures are
/// 500s (the old code mapped every update/delete error to 500, hiding a
/// real missing-job delete as "daemon broken", and every run_now error to
/// 404, hiding a real storage failure as "not found").
fn scheduler_status(e: SchedulerError) -> (StatusCode, String) {
    match &e {
        SchedulerError::NotFound(_) => (StatusCode::NOT_FOUND, e.to_string()),
        SchedulerError::Cron(_) => (StatusCode::BAD_REQUEST, e.to_string()),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Reject the request unless the calling app owns this schedule.
async fn deny_unless_owned(
    state: &AppState,
    schedule_id: &str,
    identity: &AppIdentity,
) -> Option<Response> {
    match schedules::get_schedule_for_app(&state.db, schedule_id, &identity.app_id).await {
        Ok(Some(_)) => None,
        Ok(None) => Some(err_response(
            StatusCode::NOT_FOUND,
            format!("no such schedule: {schedule_id}"),
        )),
        Err(e) => Some(err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            e.to_string(),
        )),
    }
}

pub async fn list_schedules(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    match schedules::list_schedules_for_app(&state.db, &identity.app_id).await {
        Ok(rows) => Json(json!({"schedules": rows})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn create_schedule(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Json(body): Json<Value>,
) -> Response {
    let (Some(name), Some(cron), Some(prompt)) = (
        body.get("name").and_then(|v| v.as_str()),
        body.get("cron").and_then(|v| v.as_str()),
        body.get("prompt").and_then(|v| v.as_str()),
    ) else {
        return err_response(StatusCode::BAD_REQUEST, "name, cron, prompt are required");
    };
    let enabled = body
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let mut scheduler = state.scheduler.lock().await;
    match scheduler
        .add_job(name, cron, prompt, enabled, &identity.app_id)
        .await
    {
        Ok(id) => Json(json!({"id": id})).into_response(),
        Err(e) => {
            let (status, msg) = scheduler_status(e);
            err_response(status, msg)
        }
    }
}

pub async fn update_schedule(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Some(denied) = deny_unless_owned(&state, &id, &identity).await {
        return denied;
    }
    let cron = body.get("cron").and_then(|v| v.as_str());
    let enabled = body.get("enabled").and_then(|v| v.as_bool());
    let mut scheduler = state.scheduler.lock().await;
    match scheduler.update_job(&id, cron, enabled).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => {
            let (status, msg) = scheduler_status(e);
            err_response(status, msg)
        }
    }
}

pub async fn delete_schedule(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(id): Path<String>,
) -> Response {
    if let Some(denied) = deny_unless_owned(&state, &id, &identity).await {
        return denied;
    }
    let mut scheduler = state.scheduler.lock().await;
    match scheduler.remove_job(&id).await {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => {
            let (status, msg) = scheduler_status(e);
            err_response(status, msg)
        }
    }
}

pub async fn run_now(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(id): Path<String>,
) -> Response {
    // Run the job WITHOUT the scheduler mutex: it's a potentially multi-minute
    // turn that only needs DB + agent — holding the lock across it would
    // serialize every other `POST/PATCH/DELETE /api/schedules*` (and other
    // run_nows) behind this one job for its whole run.
    // Scoped: triggering another app's schedule would run its turn, on its
    // provider, against its billing account.
    let exists = schedules::get_schedule_for_app(&state.db, &id, &identity.app_id).await;
    match exists {
        Ok(Some(_)) => {
            crate::scheduler::execute_job(&state.db, &state.agent, &id).await;
            Json(json!({"ok": true})).into_response()
        }
        // NotFound for a missing job; 500 for a real storage failure — the
        // old code collapsed both to 404.
        Ok(None) => err_response(StatusCode::NOT_FOUND, "schedule not found"),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
