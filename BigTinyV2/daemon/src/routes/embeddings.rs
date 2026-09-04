//! `POST /api/embeddings` — vectors from the daemon's in-process embedder.
//!
//! # Why this route came back
//!
//! In V1 this was a permanent 503. It had originally served the in-process
//! llama.cpp engine, adaptive-pathway consumed it over HTTP, and when AP moved
//! to embedding in-process the last caller went away — so it was left as a
//! stable "not served" answer rather than a 404.
//!
//! Meanwhile a LiteRT EmbeddingGemma model *is* loaded in the daemon, serving
//! pathway. A research pipeline and a notebook both want embeddings for
//! retrieval, and the RAM is already spent. Serving them from the same shared
//! `Arc<dyn SemanticEmbedder>` costs one route and no extra memory; making each
//! app load its own model would cost hundreds of megabytes apiece and put their
//! vectors in incomparable spaces.
//!
//! # Shape
//!
//! Two request forms, both accepted:
//!
//! - `{"prompt": "..."}` → `{"embedding": [...]}` — Ollama-compatible, kept
//!   verbatim so a client written against V1's documented shape still works.
//! - `{"input": ["...", "..."]}` → `{"embeddings": [[...], ...]}` — the batch
//!   form. A pipeline embedding ten thousand chunks one HTTP round-trip at a
//!   time is the difference between usable and not.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use super::AppState;

/// Cap on one batch. Each vector is several hundred `f32`s and embedding is
/// compute-bound, so an unbounded batch is both a large response and a
/// long-held worker.
const MAX_BATCH: usize = 256;

/// Concurrent embed calls allowed across the whole daemon.
///
/// Deliberately *not* the provider queue: that gates metered remote endpoints,
/// while this is local compute with entirely different scaling. A small
/// dedicated semaphore stops one app's bulk job from monopolising the model
/// without entangling two unrelated schedulers.
const MAX_CONCURRENT: usize = 4;

static EMBED_GATE: once_cell::sync::Lazy<Arc<tokio::sync::Semaphore>> =
    once_cell::sync::Lazy::new(|| Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT)));

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

#[derive(Debug, Deserialize)]
pub struct EmbedRequest {
    /// Ollama-compatible single-text form.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Batch form: one string, or an array of them.
    #[serde(default)]
    pub input: Option<Value>,
    /// Accepted and ignored — the daemon serves whichever model it has loaded.
    /// Echoed back so a caller can record which space its vectors are in.
    #[serde(default)]
    pub model: Option<String>,
}

/// Normalize either request form into a list of texts.
fn texts_from(req: &EmbedRequest) -> Result<Vec<String>, String> {
    if let Some(prompt) = &req.prompt {
        return Ok(vec![prompt.clone()]);
    }
    match &req.input {
        Some(Value::String(s)) => Ok(vec![s.clone()]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "every `input` item must be a string".to_string())
            })
            .collect(),
        Some(_) => Err("`input` must be a string or an array of strings".into()),
        None => Err("one of `prompt` or `input` is required".into()),
    }
}

pub async fn embed(State(state): State<Arc<AppState>>, Json(body): Json<EmbedRequest>) -> Response {
    // The same 503 V1 served, now meaning the thing it actually describes: a
    // build with no embedding model configured. "Not built with a model" stays
    // distinguishable from "wrong URL".
    let Some(embedder) = state.plugins.embedder() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "this daemon has no embedding model configured",
        );
    };

    let texts = match texts_from(&body) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    if texts.is_empty() {
        return err(StatusCode::BAD_REQUEST, "no text to embed");
    }
    if texts.len() > MAX_BATCH {
        return err(
            StatusCode::BAD_REQUEST,
            format!("batch of {} exceeds the maximum of {MAX_BATCH}", texts.len()),
        );
    }

    let _permit = match EMBED_GATE.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "embedding gate unavailable",
            )
        }
    };

    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
    for text in &texts {
        match embedder.embed(text).await {
            Some(v) => vectors.push(v),
            // A partial batch would be worse than none: the caller cannot tell
            // which row failed, and a silently short array misaligns every
            // downstream index against their own input list.
            None => {
                return err(
                    StatusCode::BAD_GATEWAY,
                    "the embedding model returned no vector",
                )
            }
        }
    }

    let dims = vectors.first().map(|v| v.len()).unwrap_or(0);
    let model = body.model.unwrap_or_else(|| "in-process".to_string());

    // Answer in the shape that was asked for: a `prompt` request gets Ollama's
    // singular `embedding`, so a client of that shape is unaffected.
    if body.prompt.is_some() {
        return Json(json!({
            "embedding": vectors.into_iter().next().unwrap_or_default(),
            "model": model,
            "dims": dims,
        }))
        .into_response();
    }

    Json(json!({ "embeddings": vectors, "model": model, "dims": dims })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(v: Value) -> EmbedRequest {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn both_request_shapes_normalize_to_texts() {
        assert_eq!(texts_from(&req(json!({"prompt": "hi"}))).unwrap(), ["hi"]);
        assert_eq!(texts_from(&req(json!({"input": "hi"}))).unwrap(), ["hi"]);
        assert_eq!(
            texts_from(&req(json!({"input": ["a", "b"]}))).unwrap(),
            ["a", "b"]
        );
    }

    #[test]
    fn a_request_naming_neither_field_is_rejected() {
        assert!(texts_from(&req(json!({}))).is_err());
    }

    #[test]
    fn a_non_string_batch_item_is_rejected_rather_than_skipped() {
        // Skipping would misalign every downstream index against the caller's
        // own list.
        assert!(texts_from(&req(json!({"input": ["a", 7]}))).is_err());
        assert!(texts_from(&req(json!({"input": 7}))).is_err());
    }

    #[test]
    fn prompt_wins_over_input_so_the_response_shape_is_unambiguous() {
        assert_eq!(
            texts_from(&req(json!({"prompt": "p", "input": ["i"]}))).unwrap(),
            ["p"]
        );
    }
}
