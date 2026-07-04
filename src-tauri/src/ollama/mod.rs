//! Ollama model management (Phase 5): list installed models, pull with live
//! progress, delete. We never call generate/chat here — inference goes through
//! goosed. Pull progress is streamed to the UI as `ollama://pull-progress`
//! events keyed by `pull_id` (supports concurrent pulls).

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
