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
use crate::error::StorageError;
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
    /// Pin the session to a specific provider/model from birth (per-session
    /// provider isolation — the sandbox/loop resolve `metadata.provider` at
    /// send time, so stamping here means the session keeps this provider even
    /// if the global default changes later). Omitted = the session follows
    /// whatever is globally active when it first sends.
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
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
    if let Some(provider) = body.provider.filter(|p| !p.is_empty()) {
        metadata["provider"] = json!(provider);
        if let Some(model) = body.model.filter(|m| !m.is_empty()) {
            metadata["model"] = json!(model);
        }
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
    // Bound the page: SQLite treats a NEGATIVE LIMIT as "no limit", so
    // `?limit=-1` used to read out the entire sessions table. The lower
    // bound stays 0 (not 1) because `?limit=0` meaning "empty page, real
    // `total`" is pinned by the route smoke tests.
    let limit = query.limit.unwrap_or(50).clamp(0, 500);
    let offset = query.offset.unwrap_or(0).max(0);
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
        // `delete_session` returns the affected row count, and discarding it
        // meant a DELETE that matched nothing still answered `{"ok": true}` —
        // the client dropped the row from its list and the session reappeared
        // on the next refresh. A miss is a 404, not a success.
        Ok(0) => err_response(StatusCode::NOT_FOUND, format!("no such session: {id}")),
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
                // `attached_paths` accumulates rather than replaces: each turn's
                // drag-and-drop attachments are added to the session's
                // approval-free read set (see `sandbox::allowed_dirs_for_session`),
                // and a later turn attaching a *different* file must not silently
                // revoke access to an earlier one. Union + dedup, order-stable.
                if k == "attached_paths" {
                    let existing = obj.get(k).and_then(|e| e.as_array()).cloned().unwrap_or_default();
                    let incoming = v.as_array().cloned().unwrap_or_default();
                    let mut merged_paths: Vec<Value> = existing;
                    for item in incoming {
                        if !merged_paths.contains(&item) {
                            merged_paths.push(item);
                        }
                    }
                    obj.insert(k.clone(), Value::Array(merged_paths));
                } else {
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
        merged
    })
    .await;

    match result {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => {
            // `update_metadata_with` reports a missing session as
            // `StorageError::NotFound` — that's a 404 for the client, not a
            // server error (which would hide the real cause from a UI that
            // treats 5xx as "daemon broken"). Match the variant, not the
            // message wording.
            let status = match &e {
                StorageError::NotFound(_) => StatusCode::NOT_FOUND,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            err_response(status, e.to_string())
        }
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
    // Push the limit into SQL (`get_last_messages_by_session`) instead of
    // fetching the whole session and truncating here — a long session used
    // to re-read every message even for tiny `limit` values.
    let result = match query.limit {
        Some(limit) => messages::get_last_messages_by_session(&state.db, &id, limit).await,
        None => messages::get_messages_by_session(&state.db, &id).await,
    };
    match result {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn get_stats(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let stats = SessionStats::new(state.db.clone());
    match stats.get_stats(&id).await {
        Ok(v) => Json(v).into_response(),
        // A genuinely-missing session is a 404; anything else (SQL error,
        // pool failure) is a real 500 — collapsing both into 404 hid real
        // infrastructure problems as "session not found" (mirror of the
        // fork_session handler's status split below). This stays a substring
        // match (unlike `update_config`'s `StorageError::NotFound` variant)
        // because `SessionStats::get_stats` is agent-side and returns a
        // plain `String` error — there is no variant to match on here.
        Err(e) if e.contains("not found") => {
            err_response(StatusCode::NOT_FOUND, e)
        }
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e),
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

    // Distinguish a genuinely-missing source session (404) from a DB error
    // (500) — the old `let Ok(Some(source)) = ... else { 404 }` collapsed a
    // real failure into a misleading "session not found".
    let source = match sessions::get_session(&state.db, &id).await {
        Ok(Some(s)) => s,
        Ok(None) => return err_response(StatusCode::NOT_FOUND, "session not found"),
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let rows = match messages::get_messages_by_session(&state.db, &id).await {
        Ok(r) => r,
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    // An unknown `at_message_id` used to fork the ENTIRE history (the lookup
    // resolved to `None`, indistinguishable from "no cutoff requested") —
    // silently producing a fork the user didn't intend. Fail loudly instead.
    let cutoff_rowid = match &body.at_message_id {
        Some(msg_id) => match rows.iter().find(|r| &r.id == msg_id) {
            Some(r) => Some(r.rowid),
            None => {
                return err_response(
                    StatusCode::NOT_FOUND,
                    "This chat cannot be forked — the selected message no \
                     longer exists in this session.",
                );
            }
        },
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
            Err(e) => {
                // Roll back the copy AND remove the session row created
                // above — a mid-loop failure used to return 500 while
                // leaving an orphaned zero-message session behind (the
                // session row and its metadata commit before this loop).
                let _ = tx.rollback().await;
                let _ = sessions::delete_session(&state.db, &new_id).await;
                return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
            }
        }
    }
    if let Err(e) = tx.commit().await {
        // Same orphan cleanup for a failed commit.
        let _ = sessions::delete_session(&state.db, &new_id).await;
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
    Path(id): Path<String>,
    Json(body): Json<ApproveRequest>,
) -> Response {
    // The path session id is part of the approval's scope: pending actions
    // are per-session, and `record_decision` resolves by action id alone —
    // so a POST to `/chat/{A}/approve` used to resolve an action pending on
    // session B. Verify the action is actually pending *for this session*
    // first. The HITL manager exposes no way to distinguish "pending for
    // another session" from "no such action" without iterating its private
    // map, so both are the same 404 here (either way, this session has no
    // such pending action).
    {
        let hitl = state.agent.hitl().lock().await;
        let pending = hitl.get_pending_approvals(&id);
        if !pending.iter().any(|p| p.action_id == body.action_id) {
            return err_response(
                StatusCode::NOT_FOUND,
                format!("no pending action {} for session {id}", body.action_id),
            );
        }
    }

    // `record_decision` is synchronous now (the DB rule-upsert was split out
    // of it) — the shared hitl mutex is held only for the in-memory mutation,
    // never across a storage round-trip, so a single approval can't serialize
    // every concurrent tool call's HITL check behind a DB write.
    let (decision, rule_to_persist) = {
        let mut hitl = state.agent.hitl().lock().await;
        hitl.record_decision(&body.action_id, &body.decision)
    };
    state.agent.resolve_approval(&body.action_id);
    if let Some(tool_name) = rule_to_persist {
        state.agent.hitl().lock().await.persist_allow_rule(&tool_name).await;
    }
    Json(decision.to_dict()).into_response()
}

// ---- POST .../compact -------------------------------------------------------

/// Manual context compaction (`/compact`): folds the session's oldest
/// un-compacted exchanges into memory, bypassing the automatic token
/// threshold (`run_compaction`'s `force`). Returns the `CompactionResult`
/// so the client can report "compacted N messages / X → Y tokens", or a
/// `{"compacted": false}` when there was nothing to fold (summarizer
/// disabled, no messages past the watermark, or another pass already holds
/// the compaction lock).
pub async fn compact_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let session = match sessions::get_session(&state.db, &id).await {
        Ok(Some(s)) => s,
        Ok(None) => return err_response(StatusCode::NOT_FOUND, "session not found"),
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let metadata: Value = session
        .metadata
        .as_ref()
        .and_then(|m| serde_json::from_str(m).ok())
        .unwrap_or(json!({}));
    let effective_provider = metadata.get("provider").and_then(|v| v.as_str());

    // Mirror `run_tool_loop`'s resolution: the session's own provider wins,
    // otherwise the router's global default, then the daemon-wide cap. A
    // failure to resolve a provider is not fatal here — compaction's token
    // budget only *defines* the high/low watermarks; fall back to the
    // configured `max_context_tokens` the way the loop does.
    let resolved_provider_id = state.agent.router().get_provider_id(effective_provider).ok();
    let context_length = resolved_provider_id
        .as_deref()
        .and_then(|pid| state.agent.router().context_length(pid))
        .unwrap_or(state.config.token_management.max_context_tokens);

    // `provider_id` is `None` when nothing resolved (no session provider, no
    // default registered) — `run_compaction` still attempts the local
    // summarizer in that case, and only needs a provider for the
    // session-model fallback leg.
    let model = resolved_provider_id
        .as_deref()
        .map(|pid| state.agent.router().resolve_model(pid, None));
    let result = crate::agent::compaction::run_compaction(
        &state.db,
        &id,
        state.agent.summarizer(),
        resolved_provider_id.as_deref(),
        model,
        &state.config.token_management,
        &state.config.summarizer,
        &state.config.memory,
        context_length,
        true,
    )
    .await;

    match result {
        Some(r) => Json(json!({
            "compacted": true,
            "messages_compacted": r.messages_compacted,
            "tokens_before": r.tokens_before,
            "tokens_after": r.tokens_after,
        }))
        .into_response(),
        None => Json(json!({"compacted": false})).into_response(),
    }
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
    // 404 only when the session genuinely doesn't exist — a DB error is a
    // 500, not another "session not found" (the old `.ok().flatten()` mapped
    // real failures to 404 too).
    match sessions::get_session(&state.db, &id).await {
        Ok(Some(_)) => {}
        Ok(None) => return err_response(StatusCode::NOT_FOUND, "session not found"),
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
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
