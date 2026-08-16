//! `POST /api/embeddings` — kept only for wire-contract stability.
//!
//! Historically this served semantic embeddings from the in-process llama.cpp
//! engine, and adaptive-pathway consumed it over HTTP. That is no longer how
//! embeddings work: AP now embeds in-process directly (LiteRT EmbeddingGemma,
//! injected as a `SemanticEmbedder` at `PathwayEngine::open_with_embedder`), so
//! nothing inside the daemon calls this route anymore. It remains as a stable
//! 503 rather than a 404 so an older external client gets a clear "not served"
//! answer instead of one indistinguishable from a wrong URL.
//!
//! The request/response shape still mirrors Ollama's (`{model, prompt}` in,
//! `{embedding: [...]}` out) should a real HTTP embedding backend ever return.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use super::AppState;

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// POST /api/embeddings
pub async fn embed(State(_state): State<Arc<AppState>>, Json(_body): Json<Value>) -> Response {
    err(
        StatusCode::SERVICE_UNAVAILABLE,
        "this daemon serves embeddings in-process (LiteRT), not over HTTP",
    )
}
