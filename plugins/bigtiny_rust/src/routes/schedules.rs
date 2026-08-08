//! `/api/schedules` routes — mirrors
//! `plugins/bigtiny/bigtiny/server/routes/schedules.py`. `create_schedule`
//! and `run_now` go through the live `Scheduler` (not just `storage::schedules`
//! directly) so a newly-created job is registered immediately rather than
//! only taking effect after the next daemon restart.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::error::SchedulerError;
use crate::storage::schedules;

use super::AppState;

fn err_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

/// Map a `SchedulerError` to the right HTTP status — a genuinely-missing
/// schedule is a 404; cron-validation and storage failures are 500s (the old
/// code mapped every update/delete error to 500, hiding a real missing-job
/// delete as "daemon broken", and every run_now error to 404, hiding a real
/// storage failure as "not found").
fn scheduler_status(e: SchedulerError) -> (StatusCode, String) {
    match &e {
        SchedulerError::NotFound(_) => (StatusCode::NOT_FOUND, e.to_string()),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn list_schedules(State(state): State<Arc<AppState>>) -> Response {
    match schedules::list_schedules(&state.db).await {
        Ok(rows) => Json(json!({"schedules": rows})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn create_schedule(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let (Some(name), Some(cron), Some(recipe_id)) = (
        body.get("name").and_then(|v| v.as_str()),
        body.get("cron").and_then(|v| v.as_str()),
        body.get("recipe_id").and_then(|v| v.as_str()),
    ) else {
        return err_response(
            StatusCode::BAD_REQUEST,
            "name, cron, recipe_id are required",
        );
    };
    let enabled = body
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let mut scheduler = state.scheduler.lock().await;
    match scheduler.add_job(name, cron, recipe_id, enabled).await {
        Ok(id) => Json(json!({"id": id})).into_response(),
        Err(e) => {
            let (status, msg) = scheduler_status(e);
            err_response(status, msg)
        }
    }
}

pub async fn update_schedule(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
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
    Path(id): Path<String>,
) -> Response {
    let mut scheduler = state.scheduler.lock().await;
    match scheduler.remove_job(&id).await {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => {
            let (status, msg) = scheduler_status(e);
            err_response(status, msg)
        }
    }
}

pub async fn run_now(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    // Run the job WITHOUT the scheduler mutex: it's a potentially multi-minute
    // recipe turn that only needs DB + recipe engine — holding the lock across
    // it would serialize every other `POST/PATCH/DELETE /api/schedules*` (and
    // other run_nows) behind this one job for its whole run.
    let exists = schedules::get_schedule(&state.db, &id).await;
    match exists {
        Ok(Some(_)) => {
            crate::scheduler::execute_job(&state.db, &state.recipe_engine, &id).await;
            Json(json!({"ok": true})).into_response()
        }
        // NotFound for a missing job; 500 for a real storage failure — the
        // old code collapsed both to 404.
        Ok(None) => err_response(StatusCode::NOT_FOUND, "schedule not found"),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
