//! Tidying an app's chats (`crate::tidy`).
//!
//! - `GET /api/apps/me/chats/stats`: every top-level chat's figures, for the
//!   client's grading.
//! - `POST /api/chat/{id}/tidy`: make `{id}` a tidy chat over the given
//!   candidates (replacing any earlier list).
//! - `POST /api/chat/{id}/tidy/readable`: the user allowed reading these refs.
//! - `GET /api/chat/{id}/tidy`: the chat's candidates and consents.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::json;

use crate::storage::apps::AppIdentity;
use crate::storage::sessions;
use crate::tidy::{self, Candidate};

use super::AppState;

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

async fn owned(state: &AppState, id: &str, identity: &AppIdentity) -> Option<Response> {
    match sessions::is_owned_by(&state.db, id, &identity.app_id).await {
        Ok(true) => None,
        Ok(false) => Some(err(StatusCode::NOT_FOUND, format!("no such session: {id}"))),
        Err(e) => Some(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

pub async fn chat_stats(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    match tidy::chat_stats(&state.db, &identity.app_id).await {
        Ok(chats) => Json(json!({ "chats": chats })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
pub struct StartRequest {
    pub candidates: Vec<Candidate>,
}

pub async fn start(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(id): Path<String>,
    Json(body): Json<StartRequest>,
) -> Response {
    if let Some(denied) = owned(&state, &id, &identity).await {
        return denied;
    }
    match tidy::validate_candidates(&state.db, &identity.app_id, &id, &body.candidates).await {
        Ok(Ok(())) => {}
        Ok(Err(why)) => return err(StatusCode::BAD_REQUEST, why.to_string()),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
    let count = body.candidates.len();
    match tidy::set_candidates(&state.db, &id, body.candidates).await {
        Ok(()) => Json(json!({ "ok": true, "candidates": count })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn get(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(id): Path<String>,
) -> Response {
    if let Some(denied) = owned(&state, &id, &identity).await {
        return denied;
    }
    match tidy::state_of(&state.db, &id).await {
        Some(s) => Json(json!(s)).into_response(),
        None => err(StatusCode::NOT_FOUND, "this chat is not tidying"),
    }
}

#[derive(Debug, Deserialize)]
pub struct ReadableRequest {
    pub refs: Vec<String>,
}

pub async fn allow_reading(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(id): Path<String>,
    Json(body): Json<ReadableRequest>,
) -> Response {
    if let Some(denied) = owned(&state, &id, &identity).await {
        return denied;
    }
    match tidy::allow_reading(&state.db, &id, &body.refs).await {
        Ok(Some(readable)) => Json(json!({ "readable": readable })).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "this chat is not tidying"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
