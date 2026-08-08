//! HTTP client for the behavioral-memory engine's `/api/pathway/*` routes —
//! linked directly into the BigTiny daemon (`plugins/adaptive-pathway_rust`),
//! reached over the same authenticated `BigTinyClient` as every other
//! daemon call. Replaces the old `crate::adaptive_pathway` module, which
//! talked to a since-retired standalone sidecar process over plain
//! unauthenticated HTTP on its own port.
//!
//! The real route surface is intentionally small: the new engine learns
//! automatically (recall + turn-end/compaction extraction happen inside the
//! daemon's agent loop, gated on `AppState.pathway` there, not reachable or
//! controllable from here) and the tool-selection-era concepts these routes
//! used to expose (edges, domains, schism, ensemble weights, graph health)
//! don't exist in the behavioral-memory design — see
//! `plugins/adaptive-pathway_rust/src/routes.rs` for what's actually served.

use serde_json::{json, Value};

use crate::bigtiny::client::BigTinyClient;

/// `GET /api/pathway/beliefs` — every belief (Settings belief browser list).
pub async fn list_beliefs(client: &BigTinyClient) -> Result<Value, String> {
    client.get_json("/api/pathway/beliefs").await
}

/// `GET /api/pathway/stats` — belief counts by layer.
pub async fn stats(client: &BigTinyClient) -> Result<Value, String> {
    client.get_json("/api/pathway/stats").await
}

/// `DELETE /api/pathway/beliefs/{id}` — belief browser's delete action.
/// Goes through `forget(reason=wrong)` semantics daemon-side (permanent
/// suppression + tombstone), not a bare row delete, so a deleted belief
/// can't silently be relearned on the next extraction pass.
pub async fn delete_belief(client: &BigTinyClient, belief_id: &str) -> Result<Value, String> {
    client
        .delete(&format!("/api/pathway/beliefs/{belief_id}"))
        .await
}

/// `PATCH /api/pathway/sessions/{id}/pause` — the incognito toggle. Paused:
/// recall returns nothing (zero prompt delta) and both learn seams skip the
/// session entirely; nothing is embedded or written.
pub async fn set_session_paused(
    client: &BigTinyClient,
    session_id: &str,
    paused: bool,
) -> Result<Value, String> {
    client
        .patch_json(
            &format!("/api/pathway/sessions/{session_id}/pause"),
            &json!({ "paused": paused }),
        )
        .await
}
