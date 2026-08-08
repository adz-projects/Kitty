//! `EmbeddingProvider`: turns arbitrary context text into a fixed-dimension
//! vector. Tries Ollama's /api/embeddings first (semantic), falling back to
//! the deterministic signed-hashing vectorizer (lexical) when Ollama is
//! unavailable or errors. Ported from `embeddings.py::EmbeddingProvider`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::Config;

use super::hashing::{hash_embed, EmbedCache};
use super::project::project;

pub struct EmbeddingProvider {
    dim: usize,
    ollama_url: String,
    ollama_model: String,
    timeout: Duration,
    probe_interval: Duration,
    client: reqwest::Client,
    state: Arc<Mutex<ProviderState>>,
}

struct ProviderState {
    available: Option<bool>, // None = never probed; Some(true/false)
    last_probe: Instant,
    cache: EmbedCache,
}

impl EmbeddingProvider {
    pub fn new(cfg: Config) -> Self {
        let ollama_url = std::env::var("AP_EMBED_OLLAMA_URL")
            .unwrap_or_else(|_| cfg.embedding.ollama_url.clone());
        let ollama_model = std::env::var("AP_EMBED_OLLAMA_MODEL")
            .unwrap_or_else(|_| cfg.embedding.ollama_model.clone());
        Self {
            dim: cfg.embedding_dim,
            ollama_url,
            ollama_model,
            timeout: Duration::from_secs(cfg.embedding.timeout_s),
            probe_interval: Duration::from_secs(cfg.embedding.probe_interval_s),
            client: reqwest::Client::new(),
            state: Arc::new(Mutex::new(ProviderState {
                available: None,
                last_probe: Instant::now() - Duration::from_secs(3600),
                cache: EmbedCache::new(cfg.embedding.cache_size),
            })),
        }
    }

    pub async fn embed(&self, text: &str) -> Vec<f32> {
        self.embed_with_space(text).await.0
    }

    /// Embed `text`, returning the vector plus whether it was produced by the
    /// semantic Ollama embedder (`true`) or the lexical signed-hash fallback
    /// (`false`). Callers that persist embeddings (learn/background/mcp) MUST
    /// tag the stored `embedding_model` with the space actually used — tagging
    /// a hash-space vector as the configured Ollama model puts it in the same
    /// pool as genuine semantic embeddings, and cosine across the two spaces
    /// is meaningless (beliefs get merged/re-called/cross-compared forever).
    pub async fn embed_with_space(&self, text: &str) -> (Vec<f32>, bool) {
        let t = text.trim();
        if t.is_empty() {
            return (vec![0.0; self.dim], false);
        }
        if let Some(v) = self.state.lock().unwrap().cache.get(t).cloned() {
            // The cache doesn't track which space produced a vector, so the
            // safest truthful answer for a cache hit is `false` (unknown) —
            // callers that need exactness (re-embed) bypass this anyway.
            return (v, false);
        }
        let (vec, semantic) = match self.embed_ollama(t).await {
            Some(v) => (v, true),
            None => (hash_embed(t, self.dim), false),
        };
        self.state.lock().unwrap().cache.put(t.to_string(), vec.clone());
        (vec, semantic)
    }

    async fn embed_ollama(&self, text: &str) -> Option<Vec<f32>> {
        {
            let st = self.state.lock().unwrap();
            match st.available {
                Some(false) if st.last_probe.elapsed() < self.probe_interval => return None,
                _ => {}
            }
        }
        let payload = serde_json::json!({
            "model": self.ollama_model,
            "prompt": text,
        });
        let url = format!("{}/api/embeddings", self.ollama_url);
        let resp = tokio::time::timeout(self.timeout, self.client.post(&url).json(&payload).send())
            .await
            .ok()?
            .ok()?;
        let data: serde_json::Value = resp.json().await.ok()?;
        let raw: Vec<f32> = data
            .get("embedding")
            .and_then(|e| serde_json::from_value(e.clone()).ok())
            .unwrap_or_default();
        if raw.is_empty() {
            self.mark_unavailable();
            return None;
        }
        self.mark_available();
        Some(project(&raw, self.dim))
    }

    fn mark_available(&self) {
        let mut st = self.state.lock().unwrap();
        st.available = Some(true);
        st.last_probe = Instant::now();
    }

    fn mark_unavailable(&self) {
        let mut st = self.state.lock().unwrap();
        st.available = Some(false);
        st.last_probe = Instant::now();
    }

    /// Force a fresh availability probe; returns whether Ollama answered.
    pub async fn probe_ollama(&self) -> bool {
        {
            let mut st = self.state.lock().unwrap();
            st.available = None;
            st.last_probe = Instant::now() - Duration::from_secs(3600);
        }
        self.embed_ollama("probe").await.is_some()
    }

    pub fn cache_len(&self) -> usize {
        self.state.lock().unwrap().cache.len()
    }
}
