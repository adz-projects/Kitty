//! HTTP client for the declarative factual-memory engine's
//! `/api/memorabilia/*` routes — linked into the BigTiny daemon
//! (`plugins/memorabilia_rust`) and reached over the same authenticated
//! `BigTinyClient` as every other daemon call. Exact parallel to
//! `crate::bigtiny::pathway`.
//!
//! Like pathway, the engine learns automatically (recall injection +
//! turn-end ingest run inside the daemon's agent loop, gated daemon-side, not
//! controllable from here). This surface exists only for the Settings pane:
//! browse the stored facts, read health counts, delete a fact, and set the
//! per-session pause the unified incognito control drives.

use serde_json::{json, Value};

use crate::bigtiny::client::BigTinyClient;

/// `GET /api/memorabilia/items` — active memory items (Settings fact browser).
pub async fn list_items(client: &BigTinyClient) -> Result<Value, String> {
    client.get_json("/api/memorabilia/items").await
}

/// `GET /api/memorabilia/stats` — counts for the health readout.
pub async fn stats(client: &BigTinyClient) -> Result<Value, String> {
    client.get_json("/api/memorabilia/stats").await
}

/// `DELETE /api/memorabilia/items/{id}` — the fact browser's delete action.
/// Goes through `forget_item` daemon-side (permanent suppression + tombstone
/// of the item's supporting evidence), not a bare row delete, so a deleted
/// fact can't be silently relearned on the next extraction pass.
pub async fn delete_item(client: &BigTinyClient, item_id: &str) -> Result<Value, String> {
    client
        .delete(&format!("/api/memorabilia/items/{item_id}"))
        .await
}

/// `POST /api/memorabilia/recover` — integrity-check the factual-memory DB
/// and, if corrupt, rebuild it in place (salvaging what still reads, leaving a
/// backup). Uses the long-timeout POST because a rebuild can take a moment.
pub async fn recover(client: &BigTinyClient) -> Result<Value, String> {
    client
        .post_json_long("/api/memorabilia/recover", &json!({}))
        .await
}

/// `PATCH /api/memorabilia/sessions/{id}/pause` — the factual-memory half of
/// Kitty's unified per-session incognito control. Paused: recall injects
/// nothing (zero prompt delta) and the turn-end ingest skips the session.
pub async fn set_session_paused(
    client: &BigTinyClient,
    session_id: &str,
    paused: bool,
) -> Result<Value, String> {
    client
        .patch_json(
            &format!("/api/memorabilia/sessions/{session_id}/pause"),
            &json!({ "paused": paused }),
        )
        .await
}
