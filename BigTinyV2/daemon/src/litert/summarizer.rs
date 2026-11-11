//! LiteRT-LM generative summarizer (Windows only) — the replacement for the
//! llama.cpp `local::LocalSummarizer`. Implements the same
//! [`adaptive_pathway::traits::StructuredChat`] seam the summarizer chain's
//! local leg used, so compaction and pathway extraction are unchanged above it.
//!
//! Windows-only by design: Android offloads compaction to the remote chat model
//! (the chain's `session_model` fallback), so no generative model runs on the
//! phone. Gated `all(windows, feature = "litert-engine")`.
//!
//! Same actor-thread rationale as the embedder ([`super::embedder`]):
//! `litert_lm_rust::Engine`/`Conversation` are non-`Send` and the `Conversation`
//! borrows the `Engine`, so the whole chain lives on one dedicated thread and
//! requests arrive over a channel. The (multi-second) model load happens on
//! that thread when the first request arrives, not at spawn, and the engine is
//! dropped again after [`IDLE_UNLOAD`] without a request: compaction is
//! occasional, and the model is the size of a small LLM in RAM or VRAM. Each
//! request runs on a fresh conversation for stateless extraction.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use adaptive_pathway::traits::StructuredChat;

use crate::agent::json_extract::{extract_json, extract_truncated_json};

/// One extraction request: the fully-rendered prompt, and a channel for the raw
/// model text (or an error string).
struct Req {
    prompt: String,
    reply: mpsc::Sender<Result<String, String>>,
    /// Set when the caller stopped waiting (its task was aborted or timed
    /// out). The actor skips a request whose caller is gone instead of
    /// generating an answer nobody will read.
    cancelled: Arc<AtomicBool>,
}

/// Drop the loaded engine after this long without a request.
const IDLE_UNLOAD: Duration = Duration::from_secs(5 * 60);

/// The longest a caller waits for one answer, queue time included. The chain
/// has its own overall deadline; this bounds the blocking-pool thread too.
const REPLY_WAIT: Duration = Duration::from_secs(600);

/// Marks the request cancelled when the waiting caller goes away, by any path.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

pub struct LiteRtSummarizer {
    // `Mutex` only for `Sync` (`StructuredChat: Send + Sync`); not held across work.
    tx: Mutex<mpsc::Sender<Req>>,
    available: bool,
}

impl LiteRtSummarizer {
    /// Spawn the actor thread for the `.litertlm` at `model_path`. Empty path =
    /// unavailable (the chain then skips straight to the router fallback).
    pub fn spawn(model_path: impl Into<String>) -> Self {
        let model_path = model_path.into();
        if model_path.trim().is_empty() {
            let (tx, _rx) = mpsc::channel();
            return Self {
                tx: Mutex::new(tx),
                available: false,
            };
        }
        let (tx, rx) = mpsc::channel::<Req>();
        thread::Builder::new()
            .name("litert-summarizer".into())
            .spawn(move || actor(model_path, rx))
            .expect("spawn litert-summarizer thread");
        Self {
            tx: Mutex::new(tx),
            available: true,
        }
    }

    pub fn is_available(&self) -> bool {
        self.available
    }
}

#[async_trait]
impl StructuredChat for LiteRtSummarizer {
    async fn structured_chat(&self, messages: Vec<Value>, schema: &Value) -> Result<Value, String> {
        if !self.available {
            return Err("litert summarizer is not configured".into());
        }
        let prompt = render_prompt(&messages, schema);
        let (rtx, rrx) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = CancelOnDrop(cancelled.clone());
        self.tx
            .lock()
            .map_err(|_| "litert summarizer lock poisoned".to_string())?
            .send(Req {
                prompt,
                reply: rtx,
                cancelled,
            })
            .map_err(|_| "litert summarizer thread is gone".to_string())?;
        let raw = tokio::task::spawn_blocking(move || rrx.recv_timeout(REPLY_WAIT))
            .await
            .map_err(|e| format!("summarizer join failed: {e}"))?
            .map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => {
                    "litert summarizer did not answer in time".to_string()
                }
                mpsc::RecvTimeoutError::Disconnected => {
                    "litert summarizer dropped the reply".to_string()
                }
            })??;

        extract_json(&raw)
            .or_else(|| extract_truncated_json(&raw))
            .ok_or_else(|| "litert summarizer produced no parseable JSON".into())
    }
}

/// Flatten the chat messages + an explicit schema instruction into a single
/// user prompt. LiteRT-LM applies the model's own chat template around it.
fn render_prompt(messages: &[Value], schema: &Value) -> String {
    let mut out = String::new();
    for m in messages {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
        let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if !content.is_empty() {
            out.push_str(role);
            out.push_str(": ");
            out.push_str(content);
            out.push_str("\n\n");
        }
    }
    out.push_str(&format!(
        "Respond with JSON only, matching this schema:\n{}",
        serde_json::to_string(schema).unwrap_or_else(|_| "{}".into())
    ));
    out
}

/// Actor thread: load the engine on the first request, answer requests on
/// fresh conversations, and drop the engine after [`IDLE_UNLOAD`] of quiet.
fn actor(model_path: String, rx: mpsc::Receiver<Req>) {
    use litert_lm_rust::Message;

    let mut engine: Option<litert_lm_rust::Engine> = None;
    loop {
        let req = if engine.is_some() {
            match rx.recv_timeout(IDLE_UNLOAD) {
                Ok(req) => req,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    tracing::info!("litert summarizer idle; unloading the model");
                    engine = None;
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(req) => req,
                Err(_) => return,
            }
        };
        if req.cancelled.load(Ordering::SeqCst) {
            continue;
        }
        if engine.is_none() {
            match load_engine(&model_path) {
                Ok(e) => engine = Some(e),
                Err(e) => {
                    // Answered, not retried in a loop: the next request tries
                    // the load again (a GPU driver may come back, a file be
                    // replaced), and the chain falls back to the provider.
                    let _ = req.reply.send(Err(format!("engine unavailable: {e}")));
                    continue;
                }
            }
        }
        let Some(eng) = engine.as_ref() else { continue };
        let result = (|| -> Result<String, String> {
            let mut conv = eng
                .create_conversation(Default::default())
                .map_err(|e| format!("conversation: {e}"))?;
            let reply = conv
                .send_message(Message::user(req.prompt.clone()))
                .map_err(|e| format!("generate: {e}"))?;
            Ok(reply_text(&reply))
        })();
        let _ = req.reply.send(result);
    }
}

/// Build the engine, GPU first with a CPU fallback. The generative
/// extraction/compaction pass is the slow path (it drove memorabilia's
/// structured-extraction timeouts), so run it on the GPU when the machine has
/// one. `enable_speculative_decoding` and `gpu_wait_for_weight_uploads` are
/// the crate's recommended GPU settings.
fn load_engine(model_path: &str) -> Result<litert_lm_rust::Engine, String> {
    use litert_lm_rust::{Backend, Engine};
    let gpu = Engine::builder(model_path)
        .backend(Backend::Gpu)
        .enable_speculative_decoding(true)
        .gpu_wait_for_weight_uploads(true)
        .build();
    match gpu {
        Ok(e) => {
            tracing::info!(model = %model_path, backend = "gpu", "litert summarizer ready");
            Ok(e)
        }
        Err(gpu_err) => {
            tracing::warn!(
                "litert summarizer GPU backend unavailable ({model_path}): {gpu_err};                  falling back to CPU"
            );
            match Engine::builder(model_path).backend(Backend::Cpu).build() {
                Ok(e) => {
                    tracing::info!(model = %model_path, backend = "cpu", "litert summarizer ready");
                    Ok(e)
                }
                Err(e) => {
                    tracing::warn!("litert summarizer engine load failed ({model_path}): {e}");
                    Err(e.to_string())
                }
            }
        }
    }
}

/// Pull plain text out of a LiteRT-LM reply, whose `content` is either a bare
/// string or an array of `{type, text}` parts.
fn reply_text(m: &litert_lm_rust::Message) -> String {
    match &m.content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        other => other.as_str().unwrap_or_default().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn render_prompt_includes_schema_and_messages() {
        let p = render_prompt(
            &[json!({"role":"user","content":"hi"})],
            &json!({"type":"object"}),
        );
        assert!(p.contains("user: hi"));
        assert!(p.contains("JSON only"));
    }

    #[test]
    fn an_empty_model_path_is_unavailable() {
        let s = LiteRtSummarizer::spawn("");
        assert!(!s.is_available());
    }
}
