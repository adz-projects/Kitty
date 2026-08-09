//! `POST /api/embeddings` — local semantic embeddings (docs/ANDROID.md D4
//! revised, §3.1).
//!
//! **The request/response shape intentionally mirrors Ollama's** endpoint of
//! the same name: `{model, prompt}` in, `{embedding: [...]}` out. Phase 2b
//! re-points adaptive-pathway at this route by changing one base URL in
//! `src-tauri/src/lifecycle/bigtiny_proc.rs`; matching the wire shape is what
//! keeps that a one-line change instead of a rename across ~40 AP call sites.
//! Do not "clean up" the field names without doing that rename first.
//!
//! Also accepts `input` as an alias for `prompt`, since that's the
//! OpenAI-style spelling and costs nothing to tolerate.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use super::AppState;

#[cfg(feature = "local-engine")]
use crate::local::{embeddings, EmbedPooling, SlotKind};

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// POST /api/embeddings
#[cfg(feature = "local-engine")]
pub async fn embed(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let cfg = state.config.local.clone();
    if !cfg.enabled {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "local engine is disabled ([local].enabled = false)",
        );
    }

    let Some(text) = body
        .get("prompt")
        .or_else(|| body.get("input"))
        .and_then(|v| v.as_str())
    else {
        return err(StatusCode::BAD_REQUEST, "`prompt` (string) is required");
    };
    let text = text.to_string();

    let slots = state.local_slots.clone();
    let pooling = EmbedPooling::parse(&cfg.embed_pooling);

    // Model load and the forward pass are both blocking and can take
    // hundreds of ms to seconds — never run them on the async runtime.
    let result = tokio::task::spawn_blocking(move || {
        let engine = slots.get_or_load(SlotKind::Embedder, &cfg)?;
        embeddings::embed_one(&engine, pooling, &text)
    })
    .await;

    match result {
        Ok(Ok(vector)) => Json(json!({ "embedding": vector })).into_response(),
        Ok(Err(e)) => {
            // A missing/unconfigured model is a *configuration* problem, and
            // saying so beats a blanket 500 — adaptive-pathway falls back to
            // hash-space either way, so this only ever reaches a human
            // reading logs or the Settings panel.
            tracing::warn!("local embedding failed: {e}");
            err(StatusCode::SERVICE_UNAVAILABLE, e.to_string())
        }
        Err(join) => {
            tracing::error!("embedding task panicked: {join}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "embedding task failed")
        }
    }
}

/// Without the `local-engine` feature the route still exists, so the wire
/// contract is stable across builds — it just reports that this daemon can't
/// serve it. A 404 here would be indistinguishable from "wrong URL".
#[cfg(not(feature = "local-engine"))]
pub async fn embed(State(_state): State<Arc<AppState>>, Json(_body): Json<Value>) -> Response {
    err(
        StatusCode::SERVICE_UNAVAILABLE,
        "this build has no local engine (feature `local-engine` is off)",
    )
}
