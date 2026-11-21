//! `/api/research` — deep research runs (`crate::research`).
//!
//! Starting a run does not run anything by itself: it ties the chat to a new
//! run and returns the instructions for turn 0, which the client sends with
//! the user's brief as an ordinary turn on that chat. That turn drafts the
//! plan and asks the user to approve it (`submit_research_plan`, an
//! always-ask tool); approval hands the run to the driver, which reports on
//! the app's event stream as `research_progress`.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::research::contract::{clamp_turns, Contract};
use crate::research::driver;
use crate::research::ops::{self, META_ROLE, META_RUN, META_WAVE, ROLE_PLAN};
use crate::research::prompts;
use crate::storage::apps::AppIdentity;
use crate::storage::research as store;
use crate::storage::sessions;

use super::AppState;

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

#[derive(Debug, Deserialize)]
pub struct StartRequest {
    pub session_id: String,
    pub brief: String,
    /// Turns, counting the waves, the critique and the write-up. Omitted: the
    /// brief's own number if it states one, else 4.
    #[serde(default)]
    pub max_turns: Option<i64>,
}

/// `POST /api/research`: tie a chat to a new run and return turn 0's
/// instructions.
pub async fn start(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Json(body): Json<StartRequest>,
) -> Response {
    let brief = body.brief.trim();
    if brief.is_empty() {
        return err(StatusCode::BAD_REQUEST, "the brief is empty");
    }
    match sessions::is_owned_by(&state.db, &body.session_id, &identity.app_id).await {
        Ok(true) => {}
        Ok(false) => return err(StatusCode::NOT_FOUND, "no such session"),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
    match store::live_run_for_session(&state.db, &body.session_id).await {
        Ok(Some(run)) => {
            return err(
                StatusCode::CONFLICT,
                format!(
                    "this chat already has a research run in progress ({})",
                    run.id
                ),
            )
        }
        Ok(None) => {}
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
    let max_turns = clamp_turns(body.max_turns);
    let id = uuid::Uuid::new_v4().to_string();
    if let Err(e) = store::create_run(
        &state.db,
        &id,
        &identity.app_id,
        &body.session_id,
        brief,
        max_turns,
    )
    .await
    {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    let tagged = sessions::update_metadata_with(&state.db, &body.session_id, |mut m| {
        if let Some(obj) = m.as_object_mut() {
            obj.insert(META_RUN.into(), json!(id));
            obj.insert(META_ROLE.into(), json!(ROLE_PLAN));
            obj.insert(META_WAVE.into(), json!(0));
        }
        m
    })
    .await;
    if let Err(e) = tagged {
        let _ = store::set_status(&state.db, &id, store::STATUS_FAILED, Some(&e.to_string())).await;
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    let instructions = prompts::plan_instructions(max_turns, body.max_turns.is_some());
    if let Ok(Some(run)) = store::get_run(&state.db, &id).await {
        state.agent.app_events().publish(
            &identity.app_id,
            driver::progress_event(&state.db, &run, None).await,
        );
    }
    Json(json!({
        "research_id": id,
        "status": store::STATUS_PLANNING,
        "max_turns": max_turns,
        "instructions": instructions,
    }))
    .into_response()
}

async fn owned_run(
    state: &AppState,
    id: &str,
    identity: &AppIdentity,
) -> Result<store::RunRow, Response> {
    match store::get_run_for_app(&state.db, id, &identity.app_id).await {
        Ok(Some(run)) => Ok(run),
        Ok(None) => Err(err(StatusCode::NOT_FOUND, "no such research run")),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// `GET /api/research/{id}`: the run, its plan and its progress.
pub async fn get(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(id): Path<String>,
) -> Response {
    let run = match owned_run(&state, &id, &identity).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let mut detail = driver::run_detail(&state.db, &run).await;
    let contract = run
        .contract
        .as_deref()
        .and_then(|c| serde_json::from_str::<Contract>(c).ok());
    if let Some(obj) = detail.as_object_mut() {
        obj.insert(
            "contract".into(),
            serde_json::to_value(&contract).unwrap_or(Value::Null),
        );
        obj.insert(
            "plan_markdown".into(),
            json!(contract.as_ref().map(Contract::to_markdown)),
        );
    }
    Json(detail).into_response()
}

#[derive(Debug, Deserialize)]
pub struct EntriesQuery {
    #[serde(default)]
    pub kind: Option<String>,
}

/// `GET /api/research/{id}/entries`: the ledger, with each entry's derived
/// status and placement.
pub async fn entries(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(id): Path<String>,
    Query(q): Query<EntriesQuery>,
) -> Response {
    if let Err(resp) = owned_run(&state, &id, &identity).await {
        return resp;
    }
    let ledger = match ops::load_ledger(&state.db, &id).await {
        Ok(l) => l,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let rows: Vec<Value> = ledger
        .entries()
        .filter(|e| q.kind.as_deref().is_none_or(|k| k == e.body.kind()))
        .map(|e| {
            json!({
                "seq": e.seq,
                "wave": e.wave,
                "kind": e.body.kind(),
                "status": format!("{:?}", ledger.status(e.seq)).to_lowercase(),
                "cells": ledger.cells_of(e.seq).collect::<Vec<_>>(),
                "sections": ledger.sections_of(e.seq).collect::<Vec<_>>(),
                "entry": serde_json::from_str::<Value>(&e.body.payload_json()).unwrap_or_default(),
            })
        })
        .collect();
    Json(json!({"entries": rows})).into_response()
}

/// `POST /api/research/{id}/cancel`: stop a run. Work in flight is cancelled;
/// the ledger stays readable.
pub async fn cancel(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(id): Path<String>,
) -> Response {
    let run = match owned_run(&state, &id, &identity).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let changed = match store::set_status(
        &state.db,
        &id,
        store::STATUS_CANCELLED,
        Some("Stopped by the user."),
    )
    .await
    {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    if changed {
        // The step running now, and the delegates under the chat.
        for step in store::steps(&state.db, &id).await.unwrap_or_default() {
            if step.status == "running" {
                if let Some(s) = step.session_id.as_deref() {
                    state.agent.cancel(s).await;
                }
                let _ =
                    store::finish_step(&state.db, &id, step.turn, "failed", Some("Stopped.")).await;
            }
        }
        if let Some(chat) = run.session_id.as_deref() {
            // Delegates running for this run are children of the chat.
            for sid in state.agent.active_session_ids() {
                if sessions::parent_of(&state.db, &sid)
                    .await
                    .ok()
                    .flatten()
                    .as_deref()
                    == Some(chat)
                {
                    state.agent.cancel(&sid).await;
                }
            }
            driver::clear_chat_role(&state.db, chat).await;
        }
        if let Ok(Some(run)) = store::get_run(&state.db, &id).await {
            let ev = driver::progress_event(&state.db, &run, None).await;
            state
                .agent
                .app_events()
                .publish(&identity.app_id, ev.clone());
            if let Some(chat) = run.session_id.as_deref() {
                state.agent.emit_to(chat, ev);
            }
        }
    }
    Json(json!({"ok": true, "cancelled": changed})).into_response()
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `GET /api/apps/me/research`: this app's runs, newest first.
pub async fn list(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Query(q): Query<ListQuery>,
) -> Response {
    let runs = match store::list_runs_for_app(
        &state.db,
        &identity.app_id,
        q.session_id.as_deref(),
        q.limit.unwrap_or(50),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let mut out = Vec::with_capacity(runs.len());
    for run in &runs {
        out.push(driver::run_detail(&state.db, run).await);
    }
    Json(json!({"runs": out})).into_response()
}
