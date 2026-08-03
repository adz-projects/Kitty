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

use crate::storage::schedules;

use super::AppState;

fn err_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
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
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
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
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn delete_schedule(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let mut scheduler = state.scheduler.lock().await;
    match scheduler.remove_job(&id).await {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn run_now(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let scheduler = state.scheduler.lock().await;
    match scheduler.run_job(&id).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_response(StatusCode::NOT_FOUND, e.to_string()),
    }
}
