//! `GET /api/search` — full-text search across the calling app's own history.
//!
//! The FTS5 index this reads has existed since migration 009, but V1 only ever
//! consulted it from `agent::memory::preflight_recall`, scoped to a single
//! session, as an internal recall step. Nothing could search *across* sessions
//! — which is exactly what a notebook wants ("where did I work out that
//! regex?") and what a pipeline wants for deduplication.
//!
//! # Scoping
//!
//! Results are filtered to sessions the calling app owns, by joining through
//! `sessions.app_id`. This is the route where a missed scope would be quietest
//! and most damaging: it returns raw conversation text, so a leak here hands
//! one app the contents of another's chats rather than merely confirming an id
//! exists. The join is therefore in the SQL rather than applied afterwards in
//! Rust, so there is no path that returns rows before filtering them.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::agent::memory::format_fts_query;
use crate::storage::apps::AppIdentity;

use super::AppState;

/// Upper bound on returned rows, whatever the caller asks for. Search results
/// carry full message content, so an unbounded page is a large response and a
/// large read; the cap keeps one query from becoming a self-inflicted DoS.
const MAX_LIMIT: i64 = 100;
const DEFAULT_LIMIT: i64 = 20;

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    /// Restrict to one session. Still ownership-checked — narrowing a search
    /// is not a way around the join.
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

fn err_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

pub async fn search(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Query(query): Query<SearchQuery>,
) -> Response {
    // Reuse the tokenizer the internal recall path uses rather than writing a
    // second one. Two formulations that drift apart would mean this route and
    // the agent's own memory disagree about what a query matches — a
    // correctness bug that would show up as "search finds it but the model
    // never recalls it", which is near-impossible to diagnose from outside.
    let fts_query = format_fts_query(&query.q);
    if fts_query.is_empty() {
        return Json(json!({"results": [], "total": 0})).into_response();
    }

    // Clamp before it reaches SQL: SQLite treats a negative LIMIT as "no
    // limit", so `?limit=-1` on an unclamped query reads the whole index.
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    let sql = r#"
        SELECT m.rowid       AS rowid,
               m.id          AS id,
               m.session_id  AS session_id,
               m.role        AS role,
               m.content     AS content,
               m.created_at  AS created_at,
               s.name        AS session_name,
               snippet(messages_fts, 0, '[', ']', '...', 24) AS snippet
        FROM messages_fts f
        JOIN messages m ON m.rowid = f.rowid
        JOIN sessions s ON s.id = m.session_id
        WHERE messages_fts MATCH ?1
          AND s.app_id = ?2
          AND (?3 IS NULL OR m.session_id = ?3)
          -- Tool dumps are noise in a human-facing search: they are large,
          -- highly repetitive, and match on incidental tokens.
          AND m.role IN ('user', 'assistant')
        ORDER BY rank
        LIMIT ?4
    "#;

    let rows = match sqlx::query(sql)
        .bind(&fts_query)
        .bind(&identity.app_id)
        .bind(query.session_id.as_deref())
        .bind(limit)
        .fetch_all(&state.db)
        .await
    {
        Ok(rows) => rows,
        // A malformed FTS5 expression is the caller's problem, not a server
        // fault: `format_fts_query` quotes terms, but a query of only
        // punctuation can still produce something FTS5 rejects.
        Err(sqlx::Error::Database(e)) if e.message().contains("fts5") => {
            return err_response(StatusCode::BAD_REQUEST, format!("invalid search query: {e}"));
        }
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let results: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<String, _>("id"),
                "session_id": r.get::<String, _>("session_id"),
                "session_name": r.get::<Option<String>, _>("session_name"),
                "role": r.get::<String, _>("role"),
                "content": r.get::<Option<String>, _>("content"),
                "snippet": r.get::<Option<String>, _>("snippet"),
                "created_at": r.get::<Option<String>, _>("created_at"),
            })
        })
        .collect();

    let total = results.len();
    Json(json!({"results": results, "total": total})).into_response()
}
