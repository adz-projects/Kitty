//! `/api/memorabilia/*` — read/mutate the declarative factual-memory engine.
//! Exact parallel to `pathway.rs`: per-app engine resolution, a disabled
//! engine is a soft 200 `{"error": ...}` (never fails the daemon), a real DB
//! failure is a 5xx. The Settings "Memorabilia" pane in Kitty is the sole
//! consumer.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::{json, Value};

use crate::storage::apps::AppIdentity;

use super::AppState;

/// Cap on the fact browser's list. The browser shows the active store; a
/// hard ceiling keeps one poll bounded as the store grows (the pane polls
/// while open). 1000 active propositions is already far more than a human
/// browses; beyond it the model's own `memorabilia_search` is the right tool.
const BROWSE_LIMIT: i64 = 1000;

fn err_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

/// Resolve the caller's own engine (per app: a fact browser must show the
/// caller its *own* store, never another app's). `None` engine (memorabilia
/// disabled for the app) is a soft error, boxed to dodge clippy's
/// `result_large_err`.
async fn engine(
    state: &AppState,
    identity: &AppIdentity,
) -> Result<Arc<memorabilia::engine::Engine>, Box<Response>> {
    state
        .memorabilia
        .memorabilia_for(&identity.app_id)
        .await
        .ok_or_else(|| Box::new(Json(json!({ "error": "memorabilia disabled" })).into_response()))
}

/// GET /api/memorabilia/items — active memory items (the Settings fact
/// browser). Propositions carry their derived confidence, importance, urgency
/// and disputed flag; the model reaches an item's full supporting evidence
/// through the `memorabilia_read_item` MCP tool, not this route.
pub async fn list_items(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    let engine = match engine(&state, &identity).await {
        Ok(e) => e,
        Err(e) => return *e,
    };
    let pool = engine.db.pool();
    let rows: Result<Vec<(String, String, f64, bool, String, String, String)>, sqlx::Error> =
        sqlx::query_as(
            "SELECT node_id, claim, confidence, is_disputed, importance, urgency, created_at \
             FROM propositions WHERE status = 'active' \
             ORDER BY confidence DESC, rowid DESC LIMIT ?",
        )
        .bind(BROWSE_LIMIT)
        .fetch_all(pool)
        .await;
    match rows {
        Ok(rows) => {
            let items: Vec<Value> = rows
                .into_iter()
                .map(|(id, claim, confidence, disputed, importance, urgency, created_at)| {
                    json!({
                        "id": id,
                        "claim": claim,
                        "confidence": confidence,
                        "disputed": disputed,
                        "importance": importance,
                        "urgency": urgency,
                        "created_at": created_at,
                    })
                })
                .collect();
            Json(json!({ "items": items, "count": items.len() })).into_response()
        }
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// GET /api/memorabilia/stats — counts for the health readout: active vs.
/// archived propositions, how many are disputed, active evidence chunks, and
/// the active count broken down by importance. Aggregate SQL, not a full
/// table materialization (the pane polls this while open).
pub async fn stats(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    let engine = match engine(&state, &identity).await {
        Ok(e) => e,
        Err(e) => return *e,
    };
    let pool = engine.db.pool();

    let result = async {
        let active: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM propositions WHERE status = 'active'")
                .fetch_one(pool)
                .await?;
        let archived: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM propositions WHERE status != 'active'",
        )
        .fetch_one(pool)
        .await?;
        let disputed: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM propositions WHERE status = 'active' AND is_disputed = 1",
        )
        .fetch_one(pool)
        .await?;
        let chunks_active: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE status = 'active'")
                .fetch_one(pool)
                .await?;
        let importance_rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT importance, COUNT(*) FROM propositions WHERE status = 'active' \
             GROUP BY importance",
        )
        .fetch_all(pool)
        .await?;
        Ok::<_, sqlx::Error>((active, archived, disputed, chunks_active, importance_rows))
    }
    .await;

    match result {
        Ok((active, archived, disputed, chunks_active, importance_rows)) => {
            let by_importance: serde_json::Map<String, Value> = importance_rows
                .into_iter()
                .map(|(k, count)| (k, json!(count)))
                .collect();
            Json(json!({
                "active": active,
                "archived": archived,
                "disputed": disputed,
                "chunks_active": chunks_active,
                "by_importance": by_importance,
            }))
            .into_response()
        }
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// DELETE /api/memorabilia/items/{id} — the fact browser's delete action.
/// Goes through `Engine::forget_item` (permanent suppression + tombstone of
/// the item's supporting evidence, so a deleted fact can't be silently
/// relearned) rather than a raw row delete.
pub async fn delete_item(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(id): Path<String>,
) -> Response {
    let engine = match engine(&state, &identity).await {
        Ok(e) => e,
        Err(e) => return *e,
    };
    let dropped = engine.forget_item(&id).await;
    Json(json!({ "id": id, "dropped": dropped })).into_response()
}

/// PATCH /api/memorabilia/sessions/{id}/pause — set the incognito/pause flag
/// for one session. Kitty drives this together with the pathway pause from a
/// single chat-header control.
pub async fn set_paused(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(id): Path<String>,
    Json(req): Json<Value>,
) -> Response {
    let engine = match engine(&state, &identity).await {
        Ok(e) => e,
        Err(e) => return *e,
    };
    let paused = req.get("paused").and_then(|v| v.as_bool()).unwrap_or(true);
    match engine.set_paused(&id, paused).await {
        Ok(()) => Json(json!({ "session_id": id, "paused": paused })).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
