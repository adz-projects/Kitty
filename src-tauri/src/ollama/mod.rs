//! Ollama model management (Phase 5): list installed models, pull with live
//! progress, delete. Inference goes through goosed — the one exception is the
//! keep-alive warm/evict calls below (Round-2 item 5), which issue an empty
//! `/api/generate` purely to pin a model in memory. Pull progress is streamed to
//! the UI as `ollama://pull-progress` events keyed by `pull_id`.

use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

fn base(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

/// `GET /api/tags` — raw model objects for the UI to render.
pub async fn list_models(base_url: &str) -> Result<Vec<Value>, String> {
    let url = format!("{}/api/tags", base(base_url));
    let resp = reqwest::get(url)
        .await
        .map_err(|e| format!("could not reach Ollama: {e}"))?;
    let json: Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(json
        .get("models")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default())
}

/// `DELETE /api/delete` — remove an installed model.
pub async fn delete_model(base_url: &str, model: &str) -> Result<(), String> {
    let url = format!("{}/api/delete", base(base_url));
    let resp = reqwest::Client::new()
        .delete(url)
        .json(&json!({ "model": model }))
        .send()
        .await
        .map_err(|e| format!("delete failed: {e}"))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("Ollama returned {}", resp.status()))
    }
}

/// `POST /api/show` — best-effort lookup of a model's max context length, for
/// suggesting (not forcing) `GOOSE_CONTEXT_LIMIT` (Round-6 Feature 1). The
/// field lives under `model_info` with an architecture-specific key name
/// (e.g. `llama.context_length`, `qwen2.context_length`, `gemma2.context_length`)
/// rather than one fixed key, so search for any key ending in `.context_length`
/// instead of hardcoding a family. Returns `None` on any failure (network,
/// missing model, missing field) — this is a suggestion, never fatal.
pub async fn show_model_context_length(base_url: &str, model: &str) -> Option<u32> {
    let url = format!("{}/api/show", base(base_url));
    let resp = reqwest::Client::new()
        .post(url)
        .json(&json!({ "model": model }))
        .send()
        .await
        .ok()?;
    let json: Value = resp.json().await.ok()?;
    let info = json.get("model_info")?.as_object()?;
    info.iter()
        .find(|(k, _)| k.ends_with(".context_length"))
        .and_then(|(_, v)| v.as_u64())
        .map(|n| n as u32)
}

/// Warm a model into Ollama's memory and pin it (Round-2 item 5): `/api/generate`
/// with an empty prompt and `keep_alive: -1` (resident until released).
pub async fn keep_alive_load(base_url: &str, model: &str) {
    warm(base_url, model, -1).await;
}

/// Evict a previously kept-alive model now (`keep_alive: 0`).
pub async fn keep_alive_release(base_url: &str, model: &str) {
    warm(base_url, model, 0).await;
}

/// Best-effort keep-alive call. Failures are ignored — this is a warm-up, not a
/// correctness-critical path (goosed still owns real inference).
async fn warm(base_url: &str, model: &str, keep_alive: i64) {
    if model.is_empty() {
        return;
    }
    let url = format!("{}/api/generate", base(base_url));
    let _ = reqwest::Client::new()
        .post(url)
        .json(&json!({ "model": model, "prompt": "", "stream": false, "keep_alive": keep_alive }))
        .send()
        .await;
}

#[derive(Clone, Serialize)]
struct PullProgress {
    pull_id: String,
    model: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed: Option<u64>,
    done: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// `POST /api/pull` (streaming NDJSON). Emits `ollama://pull-progress` per line.
pub async fn pull_model(app: AppHandle, base_url: String, model: String, pull_id: String) {
    let emit = |p: PullProgress| {
        let _ = app.emit("ollama://pull-progress", p);
    };
    let url = format!("{}/api/pull", base(&base_url));
    let resp = reqwest::Client::new()
        .post(url)
        .json(&json!({ "model": model, "stream": true }))
        .send()
        .await;

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            emit(PullProgress {
                pull_id,
                model,
                status: "error".into(),
                total: None,
                completed: None,
                done: true,
                error: Some(e.to_string()),
            });
            return;
        }
    };

    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                emit(PullProgress {
                    pull_id: pull_id.clone(),
                    model: model.clone(),
                    status: "error".into(),
                    total: None,
                    completed: None,
                    done: true,
                    error: Some(e.to_string()),
                });
                return;
            }
        };
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(nl) = buf.find('\n') {
            let line: String = buf.drain(..=nl).collect();
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
                    emit(PullProgress {
                        pull_id: pull_id.clone(),
                        model: model.clone(),
                        status: "error".into(),
                        total: None,
                        completed: None,
                        done: true,
                        error: Some(err.to_string()),
                    });
                    return;
                }
                emit(PullProgress {
                    pull_id: pull_id.clone(),
                    model: model.clone(),
                    status: v
                        .get("status")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string(),
                    total: v.get("total").and_then(|t| t.as_u64()),
                    completed: v.get("completed").and_then(|c| c.as_u64()),
                    done: false,
                    error: None,
                });
            }
        }
    }

    emit(PullProgress {
        pull_id,
        model,
        status: "success".into(),
        total: None,
        completed: None,
        done: true,
        error: None,
    });
}
