//! `/api/chat` routes. Paths, methods, and body/response shapes mirror
//! `plugins/bigtiny/bigtiny/server/routes/chat.py` exactly — Kitty's
//! existing Rust client (`src-tauri/src/bigtiny/{sessions,stream}.rs`)
//! depends on this wire shape (e.g. `POST /api/chat/` with a trailing slash,
//! `GET /api/chat/{id}/history` returning a bare array, `SSEEvent`'s field
//! names on the `/send` stream).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::agent::context::stats::SessionStats;
use crate::server::events::{serialize_sse, SSEEvent};
use crate::storage::messages::{self, MessageRow};
use crate::storage::sessions;
use crate::storage::timings;

use super::AppState;

fn err_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

// ---- POST /api/chat/ (create) & GET /api/chat/ (list) --------------------

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

pub async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateSessionRequest>,
) -> Response {
    let id = uuid::Uuid::new_v4().to_string();
    let name = body.name.unwrap_or_default();
    if let Err(e) = sessions::create_session(&state.db, &id, &name).await {
        return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    // Matches the Python original (chat.py::create_session) exactly:
    // `chat_dir` is set once here, alongside `cwd`, and never overwritten
    // again by a later `PATCH /config` that repoints `cwd` — the directory
    // sandbox (`agent/sandbox.rs::allowed_dirs_for_session`) always allows
    // this original directory even after an agentic session's `cwd` moves
    // elsewhere via "Set as working directory". Previously this was never
    // written at all, so that original directory silently fell out of the
    // sandbox's allowed set the moment `cwd` changed. `mode` also defaults
    // to `"chat"` (a real string), not a JSON `null`, matching Python.
    let mut metadata =
        json!({"mode": body.mode.filter(|m| !m.is_empty()).unwrap_or_else(|| "chat".to_string())});
    if let Some(cwd) = body.cwd.filter(|c| !c.is_empty()) {
        metadata["cwd"] = json!(cwd);
        metadata["chat_dir"] = json!(cwd);
    }
    if let Err(e) = sessions::update_session_config(&state.db, &id, &metadata.to_string()).await {
        tracing::warn!("failed to save initial metadata for session {id}: {e}");
    }

    Json(json!({"session_id": id})).into_response()
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

pub async fn list_sessions(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(50);
    let offset = query.offset.unwrap_or(0);
    match sessions::list_sessions_page(&state.db, limit, offset).await {
        Ok((rows, total)) => Json(json!({"sessions": rows, "total": total})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ---- PATCH /api/chat/{id} (rename) & DELETE ------------------------------

#[derive(Debug, Deserialize)]
pub struct RenameRequest {
    pub name: String,
}

pub async fn rename_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<RenameRequest>,
) -> Response {
    match sessions::update_session_name(&state.db, &id, &body.name).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    state.agent.cancel(&id).await;
    match sessions::delete_session(&state.db, &id).await {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ---- PATCH /api/chat/{id}/config -----------------------------------------

/// Shallow-merges the request body's top-level keys into the session's
/// metadata JSON (`cwd`, `mode`, `provider`, `model`, etc.) — a PATCH, not a
/// replace, matching Python's `update_session_config` route behavior.
pub async fn update_config(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(patch): Json<Value>,
) -> Response {
    let Some(patch_obj) = patch.as_object().cloned() else {
        return err_response(
            StatusCode::BAD_REQUEST,
            "config patch must be a JSON object",
        );
    };

    // Atomic read-modify-write, not a separate get + update — this races
    // against `SessionStats::record_usage` (fires after nearly every LLM
    // call) on the same `metadata` column otherwise.
    let result = sessions::update_metadata_with(&state.db, &id, move |mut merged| {
        if let Some(obj) = merged.as_object_mut() {
            for (k, v) in &patch_obj {
                obj.insert(k.clone(), v.clone());
            }
        }
        merged
    })
    .await;

    match result {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ---- GET .../history, .../stats, .../timings, .../pending ----------------

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    #[serde(default)]
    pub limit: Option<i64>,
}

pub async fn get_history(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Response {
    match messages::get_messages_by_session(&state.db, &id).await {
        Ok(mut rows) => {
            if let Some(limit) = query.limit {
                if rows.len() as i64 > limit {
                    let start = rows.len() - limit as usize;
                    rows = rows.split_off(start);
                }
            }
            Json(rows).into_response()
        }
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn get_stats(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let stats = SessionStats::new(state.db.clone());
    match stats.get_stats(&id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(StatusCode::NOT_FOUND, e),
    }
}

pub async fn get_timings(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match timings::get_recent_timings(&state.db, &id, 50).await {
        Ok(rows) => Json(json!({"timings": rows})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn get_pending(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let hitl = state.agent.hitl().lock().await;
    let pending: Vec<Value> = hitl
        .get_pending_approvals(&id)
        .iter()
        .map(|p| p.to_dict())
        .collect();
    Json(json!({"pending": pending})).into_response()
}

// ---- POST .../fork ---------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct ForkRequest {
    #[serde(default)]
    pub at_message_id: Option<String>,
}

pub async fn fork_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<ForkRequest>>,
) -> Response {
    let body = body.map(|Json(b)| b).unwrap_or_default();

    let Ok(Some(source)) = sessions::get_session(&state.db, &id).await else {
        return err_response(StatusCode::NOT_FOUND, "session not found");
    };
    let rows = match messages::get_messages_by_session(&state.db, &id).await {
        Ok(r) => r,
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let cutoff_rowid = match &body.at_message_id {
        Some(msg_id) => rows.iter().find(|r| &r.id == msg_id).map(|r| r.rowid),
        None => None,
    };
    let kept: Vec<&MessageRow> = rows
        .iter()
        .filter(|r| cutoff_rowid.map(|c| r.rowid <= c).unwrap_or(true))
        .collect();

    let new_id = uuid::Uuid::new_v4().to_string();
    if let Err(e) =
        sessions::create_session(&state.db, &new_id, source.name.as_deref().unwrap_or("")).await
    {
        return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    if let Some(meta) = &source.metadata {
        if let Err(e) = sessions::update_session_config(&state.db, &new_id, meta).await {
            tracing::warn!("failed to copy metadata into forked session {new_id}: {e}");
        }
    }

    let mut tx = match state.db.begin().await {
        Ok(tx) => tx,
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    // `compacted_through_rowid` is a rowid into the shared `messages` table,
    // meaningless once copied verbatim into the new session's freshly
    // assigned rows — track which *new* rowid corresponds to the source's
    // compaction boundary as we insert, so it can be remapped below instead
    // of just dropped.
    let mut new_compacted_through_rowid: Option<i64> = None;
    for row in kept {
        let new_msg_id = uuid::Uuid::new_v4().to_string();
        let res = sqlx::query(
            r#"INSERT INTO messages (id, session_id, role, content, tool_calls, tool_call_id, token_count, content_format)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&new_msg_id)
        .bind(&new_id)
        .bind(&row.role)
        .bind(&row.content)
        .bind(&row.tool_calls)
        .bind(&row.tool_call_id)
        .bind(row.token_count.unwrap_or(0))
        .bind(&row.content_format)
        .execute(&mut *tx)
        .await;
        match res {
            Ok(result) if row.rowid == source.compacted_through_rowid => {
                new_compacted_through_rowid = Some(result.last_insert_rowid());
            }
            Ok(_) => {}
            Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    }
    if let Err(e) = tx.commit().await {
        return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    // Only carry the compaction boundary + summary over when the message
    // that boundary points at actually made it into this fork (a truncated
    // fork — `at_message_id` earlier than the compaction point — leaves the
    // new session with no valid boundary to remap to, matching the previous
    // "just drop it" behavior rather than pointing at the wrong message or a
    // stale summary that covers messages the fork no longer has).
    if let (Some(new_rowid), Some(memory_slots)) =
        (new_compacted_through_rowid, &source.memory_slots)
    {
        if let Err(e) =
            sessions::update_compaction_state(&state.db, &new_id, memory_slots, new_rowid).await
        {
            tracing::warn!("failed to copy compaction state into forked session {new_id}: {e}");
        }
    }

    Json(json!({"session_id": new_id})).into_response()
}

// ---- POST .../cancel -------------------------------------------------------

pub async fn cancel_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    state.agent.cancel(&id).await;
    if let Err(e) = sessions::update_session_status(&state.db, &id, "idle").await {
        tracing::warn!("failed to mark session {id} idle after cancel: {e}");
    }
    Json(json!({"ok": true})).into_response()
}

// ---- POST .../approve -------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ApproveRequest {
    pub action_id: String,
    pub decision: String,
}

pub async fn approve_action(
    State(state): State<Arc<AppState>>,
    Path(_id): Path<String>,
    Json(body): Json<ApproveRequest>,
) -> Response {
    let decision = {
        let mut hitl = state.agent.hitl().lock().await;
        hitl.record_decision(&body.action_id, &body.decision).await
    };
    state.agent.resolve_approval(&body.action_id);
    Json(decision.to_dict()).into_response()
}

// ---- POST .../send (SSE) ---------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub message: String,
    #[serde(default)]
    pub images: Option<Vec<Value>>,
}

pub async fn send_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<SendMessageRequest>,
) -> Response {
    if sessions::get_session(&state.db, &id)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return err_response(StatusCode::NOT_FOUND, "session not found");
    }

    let (tx, rx) = mpsc::unbounded_channel::<SSEEvent>();
    if let Err(e) = state
        .agent
        .run_turn(id, body.message, body.images, None, tx)
    {
        return err_response(StatusCode::CONFLICT, e);
    }

    let stream = UnboundedReceiverStream::new(rx)
        .map(|event| Ok::<Bytes, std::convert::Infallible>(Bytes::from(serialize_sse(&event))));

    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert(
        "Content-Type",
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert("Cache-Control", HeaderValue::from_static("no-cache"));
    response
}
