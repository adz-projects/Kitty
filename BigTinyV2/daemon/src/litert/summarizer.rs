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
//! requests arrive over a channel. The (multi-second) model load happens once on
//! that thread; each request runs on a fresh conversation for stateless
//! extraction.

use std::sync::{mpsc, Mutex};
use std::thread;

use async_trait::async_trait;
use serde_json::Value;

use adaptive_pathway::traits::StructuredChat;

use crate::agent::json_extract::extract_json;

/// One extraction request: the fully-rendered prompt, and a channel for the raw
/// model text (or an error string).
struct Req {
    prompt: String,
    reply: mpsc::Sender<Result<String, String>>,
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
        self.tx
            .lock()
            .map_err(|_| "litert summarizer lock poisoned".to_string())?
            .send(Req { prompt, reply: rtx })
            .map_err(|_| "litert summarizer thread is gone".to_string())?;
        let raw = tokio::task::spawn_blocking(move || rrx.recv())
            .await
            .map_err(|e| format!("summarizer join failed: {e}"))?
            .map_err(|_| "litert summarizer dropped the reply".to_string())??;

        extract_json(&raw).ok_or_else(|| "litert summarizer produced no parseable JSON".into())
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

/// Actor thread: load the engine once, answer requests on fresh conversations.
fn actor(model_path: String, rx: mpsc::Receiver<Req>) {
    use litert_lm_rust::{Backend, Engine, Message};

    let engine = match Engine::builder(&model_path).backend(Backend::Cpu).build() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("litert summarizer engine load failed ({model_path}): {e}");
            for req in rx.iter() {
                let _ = req.reply.send(Err("engine unavailable".into()));
            }
            return;
        }
    };
    tracing::info!(model = %model_path, "litert summarizer ready");

    for req in rx.iter() {
        let result = (|| -> Result<String, String> {
            let mut conv = engine
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
