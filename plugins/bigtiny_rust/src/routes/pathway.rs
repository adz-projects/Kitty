//! `/api/pathway/*` — read/mutate the behavioral-memory engine. Follows
//! `memory.rs`'s shape/cadence. `None` engine (pathway disabled) returns
//! 404-style errors rather than failing the daemon.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use serde_json::{json, Value};

use super::AppState;

fn engine(state: &AppState) -> Result<Arc<adaptive_pathway::engine::PathwayEngine>, Value> {
    state
        .pathway
        .as_ref()
        .cloned()
        .ok_or_else(|| json!({ "error": "pathway disabled" }))
}

/// GET /api/pathway/beliefs — all beliefs (for the Settings belief browser).
pub async fn list_beliefs(State(state): State<Arc<AppState>>) -> Json<Value> {
    let engine = match engine(&state) {
        Ok(e) => e,
        Err(e) => return Json(e),
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
            Json(json!({ "beliefs": items, "count": items.len() }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

/// GET /api/pathway/stats — belief counts by layer + suppression counts.
pub async fn stats(State(state): State<Arc<AppState>>) -> Json<Value> {
    let engine = match engine(&state) {
        Ok(e) => e,
        Err(e) => return Json(e),
    };
    let beliefs = engine.db.list_beliefs(None).await.unwrap_or_default();
    let mut by_layer: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for b in &beliefs {
        *by_layer.entry(b.layer.as_str().to_string()).or_insert(0) += 1;
    }
    Json(json!({ "total": beliefs.len(), "by_layer": by_layer }))
}

/// DELETE /api/pathway/beliefs/{id} — the Settings belief browser's delete
/// action. Goes through the same `forget` semantics the model's `forget`
/// tool uses (default `reason=wrong`: permanent suppression + tombstone, not
/// just a bare row delete, so a deleted belief can't silently be relearned
/// on the next extraction pass) rather than a raw `DELETE FROM beliefs`.
pub async fn delete_belief(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<Value> {
    let engine = match engine(&state) {
        Ok(e) => e,
        Err(e) => return Json(e),
    };
    use adaptive_pathway::store::suppressions::SuppressReason;
    match engine.db.forget_belief_by_id(&id, SuppressReason::Wrong).await {
        Ok(Some(dropped)) => Json(json!({ "id": id, "dropped": dropped })),
        Ok(None) => Json(json!({ "error": "belief not found" })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

/// PATCH /api/pathway/sessions/{id}/pause — set the incognito/pause flag.
pub async fn set_paused(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<Value>,
) -> Json<Value> {
    let engine = match engine(&state) {
        Ok(e) => e,
        Err(e) => return Json(e),
    };
    let paused = req.get("paused").and_then(|v| v.as_bool()).unwrap_or(true);
    match engine.set_paused(&id, paused).await {
        Ok(()) => Json(json!({ "session_id": id, "paused": paused })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}
