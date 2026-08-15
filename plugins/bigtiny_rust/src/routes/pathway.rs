//! `/api/pathway/*` — read/mutate the behavioral-memory engine. Follows
//! `memory.rs`'s shape/cadence. `None` engine (pathway disabled) returns
//! 404-style errors rather than failing the daemon.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use super::AppState;

fn err_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

/// A disabled pathway is a soft 200 `{"error": ...}` (matching
/// `memory::stats`'s "never fail the daemon" shape, and pinned by the route
/// smoke tests) — distinct from a REAL failure below, which is a 5xx.
/// Boxed: a bare `Response` in the Err slot trips clippy's
/// `result_large_err`.
fn engine(
    state: &AppState,
) -> Result<Arc<adaptive_pathway::engine::PathwayEngine>, Box<Response>> {
    state
        .pathway
        .as_ref()
        .cloned()
        .ok_or_else(|| Box::new(Json(json!({ "error": "pathway disabled" })).into_response()))
}

/// GET /api/pathway/beliefs — all beliefs (for the Settings belief browser).
pub async fn list_beliefs(State(state): State<Arc<AppState>>) -> Response {
    let engine = match engine(&state) {
        Ok(e) => e,
        Err(e) => return *e,
    };
    match engine.db.list_beliefs(None).await {
        Ok(beliefs) => {
            let items: Vec<Value> = beliefs
                .iter()
                .map(|b| {
                    json!({
                        "id": b.id,
                        "text": b.text,
                        "layer": b.layer.as_str(),
                        "confidence": b.confidence,
                        "tested": b.tested,
                        "domain": b.domain,
                        "support_count": b.support_count,
                        "distinct_sessions": b.distinct_sessions,
                        "contradict_count": b.contradict_count,
                        "pinned": b.pinned,
                    })
                })
                .collect();
            Json(json!({ "beliefs": items, "count": items.len() })).into_response()
        }
        // Was a 200 `{"error": ...}` — a DB failure reported as a successful
        // poll is indistinguishable from "no beliefs yet" to the caller.
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// GET /api/pathway/stats — belief counts by layer + embedding-migration
/// progress (`embedding_migration`: how many beliefs are still tagged with a
/// stale embedding model, and what the current one is — see
/// `migrations/005_belief_embedding_model.sql` and
/// `background::reembed_stale_beliefs` in `adaptive-pathway_rust`).
///
/// Computed with aggregate SQL, not by materializing the whole beliefs
/// table per poll the way the old `list_beliefs(None)` version did — the
/// Settings pane polls this while open, and the table only grows.
pub async fn stats(State(state): State<Arc<AppState>>) -> Response {
    let engine = match engine(&state) {
        Ok(e) => e,
        Err(e) => return *e,
    };
    let pool = engine.db.pool();
    let current_model = engine.cfg.embedding.ollama_model.clone();

    let result = async {
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM beliefs")
            .fetch_one(pool)
            .await?;
        let layer_rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT layer, COUNT(*) FROM beliefs GROUP BY layer")
                .fetch_all(pool)
            .await?;
        let pending_reembed: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM beliefs WHERE embedding_model != ?1")
                .bind(&current_model)
                .fetch_one(pool)
                .await?;
        Ok::<_, sqlx::Error>((total, layer_rows, pending_reembed))
    }
    .await;

    match result {
        Ok((total, layer_rows, pending_reembed)) => {
            let by_layer: serde_json::Map<String, Value> = layer_rows
                .into_iter()
                .map(|(layer, count)| (layer, json!(count)))
                .collect();
            Json(json!({
                "total": total,
                "by_layer": by_layer,
                "embedding_migration": {
                    "pending": pending_reembed,
                    "current_model": current_model,
                },
            }))
            .into_response()
        }
        // Was `unwrap_or_default()`: a DB error used to be reported as
        // fabricated all-zero stats — indistinguishable from a fresh,
        // genuinely-empty engine on the dashboard.
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// DELETE /api/pathway/beliefs/{id} — the Settings belief browser's delete
/// action. Goes through the same `forget` semantics the model's `forget`
/// tool uses (default `reason=wrong`: permanent suppression + tombstone, not
/// just a bare row delete, so a deleted belief can't silently be relearned
/// on the next extraction pass) rather than a raw `DELETE FROM beliefs`.
pub async fn delete_belief(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let engine = match engine(&state) {
        Ok(e) => e,
        Err(e) => return *e,
    };
    use adaptive_pathway::store::suppressions::SuppressReason;
    match engine.db.forget_belief_by_id(&id, SuppressReason::Wrong).await {
        Ok(Some(dropped)) => Json(json!({ "id": id, "dropped": dropped })).into_response(),
        // A genuinely-missing belief stays a soft 200 error (pinned by the
        // route smoke tests); a real DB failure is a 500.
        Ok(None) => Json(json!({ "error": "belief not found" })).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// PATCH /api/pathway/sessions/{id}/pause — set the incognito/pause flag.
pub async fn set_paused(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<Value>,
) -> Response {
    let engine = match engine(&state) {
        Ok(e) => e,
        Err(e) => return *e,
    };
    let paused = req.get("paused").and_then(|v| v.as_bool()).unwrap_or(true);
    match engine.set_paused(&id, paused).await {
        Ok(()) => Json(json!({ "session_id": id, "paused": paused })).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
