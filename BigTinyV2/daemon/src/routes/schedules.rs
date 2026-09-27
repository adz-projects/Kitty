//! `/api/schedules` routes. `create`/`update`/`delete` go through the live
//! `Scheduler` (not just `storage::schedules`) so a change takes effect on the
//! running triggers immediately rather than after the next daemon restart.
//!
//! A schedule is `cron`, `interval` or `once` (see `scheduler` and migration
//! 025). Every route is scoped to the calling app: a schedule runs a turn on
//! one app's behalf, against its provider and its billing account.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::SchedulerError;
use crate::scheduler::StartRunError;
use crate::storage::apps::AppIdentity;
use crate::storage::execution;
use crate::storage::schedules::{self, ScheduleRow, ScheduleSpec, KIND_CRON};

use super::AppState;

fn err_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

/// Map a `SchedulerError` to the right HTTP status — a genuinely-missing
/// schedule is a 404; a validation failure is the caller's bad input (400,
/// not a 500 that reads as "daemon broken"); storage failures are 500s.
fn scheduler_status(e: SchedulerError) -> (StatusCode, String) {
    match &e {
        SchedulerError::NotFound(_) => (StatusCode::NOT_FOUND, e.to_string()),
        SchedulerError::Cron(_) => (StatusCode::BAD_REQUEST, e.to_string()),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// The calling app's schedule, or the response to send instead.
async fn owned(
    state: &AppState,
    schedule_id: &str,
    identity: &AppIdentity,
) -> Result<ScheduleRow, Response> {
    match schedules::get_schedule_for_app(&state.db, schedule_id, &identity.app_id).await {
        Ok(Some(row)) => Ok(row),
        Ok(None) => Err(err_response(
            StatusCode::NOT_FOUND,
            format!("no such schedule: {schedule_id}"),
        )),
        Err(e) => Err(err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// Merge a request body over `base`. Only keys present in the body change;
/// `null` clears an optional field.
fn merge_spec(mut base: ScheduleSpec, body: &Value) -> ScheduleSpec {
    let s = |k: &str| body.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let opt = |k: &str, cur: Option<String>| match body.get(k) {
        Some(Value::Null) => None,
        Some(v) => v.as_str().map(str::to_string).or(cur),
        None => cur,
    };
    if let Some(v) = s("name") {
        base.name = v;
    }
    if let Some(v) = s("prompt") {
        base.prompt = v;
    }
    if let Some(v) = s("kind") {
        base.kind = v;
    }
    if let Some(v) = s("cron") {
        base.cron = v;
    }
    match body.get("interval_secs") {
        Some(Value::Null) => base.interval_secs = None,
        Some(v) => base.interval_secs = v.as_i64().or(base.interval_secs),
        None => {}
    }
    base.run_at = opt("run_at", base.run_at);
    base.provider_id = opt("provider_id", base.provider_id);
    base.model = opt("model", base.model);
    base.system_prompt = opt("system_prompt", base.system_prompt);
    base.cwd = opt("cwd", base.cwd);
    base.first_run_at = s("first_run_at");
    if let Some(v) = body.get("hitl_timeout_secs").and_then(|v| v.as_i64()) {
        base.hitl_timeout_secs = v;
    }
    if let Some(v) = body.get("enabled").and_then(|v| v.as_bool()) {
        base.enabled = v;
    }
    base
}

/// A schedule as the API returns it: the row plus when it next fires.
async fn with_next_run(state: &AppState, row: ScheduleRow) -> Value {
    let next = state.scheduler.lock().await.next_run(&row).await;
    let mut v = serde_json::to_value(&row).unwrap_or_else(|_| json!({}));
    v["next_run_at"] = json!(next);
    v
}

pub async fn list_schedules(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    match schedules::list_schedules_for_app(&state.db, &identity.app_id).await {
        Ok(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                out.push(with_next_run(&state, row).await);
            }
            Json(json!({"schedules": out})).into_response()
        }
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `POST /api/schedules`. Required: `name`, `prompt`, and the timing field
/// for `kind` (default `cron`): `cron`, `interval_secs` or `run_at`.
/// Optional: `provider_id`, `model`, `system_prompt`, `cwd`, `hitl_timeout_secs`
/// (default 600), `enabled` (default true).
pub async fn create_schedule(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Json(body): Json<Value>,
) -> Response {
    let spec = merge_spec(
        ScheduleSpec {
            kind: KIND_CRON.to_string(),
            hitl_timeout_secs: 600,
            enabled: true,
            ..Default::default()
        },
        &body,
    );
    let mut scheduler = state.scheduler.lock().await;
    match scheduler.add_schedule(&identity.app_id, spec).await {
        Ok(id) => Json(json!({"id": id})).into_response(),
        Err(e) => {
            let (status, msg) = scheduler_status(e);
            err_response(status, msg)
        }
    }
}

/// `PATCH /api/schedules/{id}`. Any field `create` takes; absent fields are
/// unchanged. Editing anything but the timing leaves the next run where it
/// was.
pub async fn update_schedule(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let current = match owned(&state, &id, &identity).await {
        Ok(row) => row,
        Err(resp) => return resp,
    };
    let spec = merge_spec(ScheduleSpec::from_row(&current), &body);
    let mut scheduler = state.scheduler.lock().await;
    match scheduler.update_schedule(&id, spec).await {
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
    if let Err(resp) = owned(&state, &id, &identity).await {
        return resp;
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

/// `POST /api/schedules/{id}/run_now` — start a run now and return its
/// `session_id` at once; the run continues in the background, reporting on the
/// app's event stream like a timed run. 409 while a run of this schedule is
/// already in progress, or when the schedule is disabled.
pub async fn run_now(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(id): Path<String>,
) -> Response {
    // Scoped: triggering another app's schedule would run its turn, on its
    // provider, against its billing account.
    if let Err(resp) = owned(&state, &id, &identity).await {
        return resp;
    }
    match crate::scheduler::start_job_now(&state.db, &state.agent, &id).await {
        Ok(session_id) => Json(json!({"ok": true, "session_id": session_id})).into_response(),
        Err(StartRunError::NotFound) => err_response(StatusCode::NOT_FOUND, "schedule not found"),
        Err(StartRunError::Disabled) => {
            err_response(StatusCode::CONFLICT, "the schedule is disabled")
        }
        Err(StartRunError::AlreadyRunning) => err_response(
            StatusCode::CONFLICT,
            "a run of this schedule is already in progress",
        ),
        Err(StartRunError::Storage(e)) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Debug, Deserialize)]
pub struct RunsQuery {
    #[serde(default = "default_runs_limit")]
    limit: i64,
}

fn default_runs_limit() -> i64 {
    20
}

/// `GET /api/schedules/{id}/runs?limit=` — this schedule's runs, newest first.
pub async fn runs(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(id): Path<String>,
    Query(q): Query<RunsQuery>,
) -> Response {
    if let Err(resp) = owned(&state, &id, &identity).await {
        return resp;
    }
    match execution::get_executions_for_trigger(&state.db, &id, q.limit.clamp(1, 200)).await {
        Ok(rows) => Json(json!({"runs": rows})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
