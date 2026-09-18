//! Semantic embedding provider with deterministic lexical fallback
//! (plan §3.3).
//!
//! [`OllamaEmbedder`] turns text into a fixed-dimension vector. It tries the
//! pinned semantic model (Ollama over HTTP, `embedding_provider_url`) within
//! the `embedding_timeout_ms` budget; on any failure — dead socket, timeout,
//! model error, empty result — it produces the deterministic signed-hash
//! vector instead, so ingestion never hard-fails (soft-fail, claude.md
//! principle 9).
//!
//! The two spaces are incompatible (plan §3.3). Every result therefore
//! carries the tag of the space that *actually* produced it — persisting a
//! hash-space vector tagged as the semantic model would put it in the same
//! recall/cluster pool as genuine semantic vectors, and cosine across the
//! two spaces is meaningless. Callers persist and filter by the returned
//! tag, never by the embedder's static configuration.
//!
//! Availability bookkeeping mirrors the behavioral-memory reference's
//! `embed/provider.rs`: a probe-interval circuit-breaker so that while the
//! service is down each embed fast-falls back instead of paying the timeout
//! budget, and an LRU text cache keyed on the exact trimmed text that stores
//! the space tag alongside the vector.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use std::result::Result;

use async_trait::async_trait;

use crate::config::{Config, HASH_EMBED_MODEL};
use crate::traits::Embedder;

use super::hashing::hash_embed;
use super::project::project;

/// LRU cache keyed on the exact (trimmed) text. Stores the space tag
/// alongside each vector so a cache hit reports the space that actually
/// produced it, instead of a hardcoded guess — a hash-fallback vector cached
/// during an outage must still read back as `__lexical_hash__` once the
/// service recovers, or `reembed_stale` would either mistag it as semantic
/// or perpetually skip text that is re-embeddable.
pub struct EmbedCache {
    cap: usize,
    map: HashMap<String, (Vec<f32>, String)>,
    order: VecDeque<String>,
}

impl EmbedCache {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn get(&self, text: &str) -> Option<&(Vec<f32>, String)> {
        self.map.get(text)
    }

    pub fn put(&mut self, text: String, vec: Vec<f32>, model: String) {
        if self.map.contains_key(&text) {
            self.order.retain(|t| *t != text);
        }
        self.map.insert(text.clone(), (vec, model));
        self.order.push_back(text);
        while self.order.len() > self.cap {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

struct ProviderState {
    /// `None` = never probed; `Some(false)` = down since `last_probe`.
    available: Option<bool>,
    last_probe: Instant,
    cache: EmbedCache,
}

pub struct OllamaEmbedder {
    dim: usize,
    /// Ollama endpoint (`embedding_provider_url`).
    url: String,
    /// Model tag of the semantic space; also the Ollama model name
    /// (`embedding_model`).
    model: String,
    /// Per-call budget (`embedding_timeout_ms`).
    timeout: Duration,
    /// Circuit-breaker re-probe gap (`embedding_probe_interval_s`).
    probe_interval: Duration,
    client: reqwest::Client,
    state: Arc<Mutex<ProviderState>>,
}

impl OllamaEmbedder {
    pub fn new(cfg: &Config) -> Self {
        Self {
            dim: cfg.embedding_dim,
            url: cfg.embedding_provider_url.trim_end_matches('/').to_string(),
            model: cfg.embedding_model.clone(),
            timeout: Duration::from_millis(cfg.embedding_timeout_ms),
            probe_interval: Duration::from_secs(cfg.embedding_probe_interval_s),
            client: reqwest::Client::new(),
            state: Arc::new(Mutex::new(ProviderState {
                available: None,
                last_probe: Instant::now() - Duration::from_secs(3600),
                cache: EmbedCache::new(cfg.embedding_cache_size),
            })),
        }
    }

    /// Force a fresh availability probe; returns whether the semantic embedder
    /// answered. `reembed_stale` always probes before re-embedding a
    /// `__lexical_hash__` row (plan §3.3).
    pub async fn probe_semantic(&self) -> bool {
        {
            let mut st = self.state.lock().unwrap();
            st.available = None;
            st.last_probe = Instant::now() - Duration::from_secs(3600);
        }
        self.embed_semantic("probe").await.is_some()
    }

    pub fn cache_len(&self) -> usize {
        self.state.lock().unwrap().cache.len()
    }

    /// The one semantic path: circuit-breaker gate, raw fetch, projection,
    /// availability bookkeeping.
    async fn embed_semantic(&self, text: &str) -> Option<Vec<f32>> {
        {
            let st = self.state.lock().unwrap();
            if matches!(st.available, Some(false)) && st.last_probe.elapsed() < self.probe_interval
            {
                return None;
            }
        }
        match self.fetch_ollama(text).await {
            Some(v) if !v.is_empty() => {
                self.mark_available();
                Some(project(&v, self.dim))
            }
            _ => {
                self.mark_unavailable();
                None
            }
        }
    }

    /// Raw HTTP fetch — no projection, no availability marking; that's
    /// [`Self::embed_semantic`]'s job.
    async fn fetch_ollama(&self, text: &str) -> Option<Vec<f32>> {
        let url = format!("{}/api/embeddings", self.url);
        let payload = serde_json::json!({
            "model": self.model,
            "prompt": text,
        });
        let resp = tokio::time::timeout(
            self.timeout,
            self.client.post(&url).json(&payload).send(),
        )
        .await
        .ok()?
        .ok()?;
        let data: serde_json::Value = resp.json().await.ok()?;
        data.get("embedding")
            .and_then(|e| serde_json::from_value::<Vec<f32>>(e.clone()).ok())
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
}

#[async_trait]
impl Embedder for OllamaEmbedder {
    async fn embed(&self, text: &str) -> Result<(Vec<f32>, String), String> {
        let t = text.trim();
        if t.is_empty() {
            return Ok((vec![0.0f32; self.dim], HASH_EMBED_MODEL.into()));
        }
        if let Some((v, tag)) = self.state.lock().unwrap().cache.get(t).cloned() {
            return Ok((v, tag));
        }
        let (vec, tag) = match self.embed_semantic(t).await {
            Some(v) => (v, self.model.clone()),
            None => (hash_embed(t, self.dim), HASH_EMBED_MODEL.into()),
        };
        self.state
            .lock()
            .unwrap()
            .cache
            .put(t.to_string(), vec.clone(), tag.clone());
        Ok((vec, tag))
    }

    /// Cache-bypassing embed (plan §3.3 `reembed_stale`): the normal path is
    /// satisfied by a hash-fallback entry cached during a prior outage forever,
    /// since nothing else invalidates it — the exact rows the re-embed pass
    /// exists to fix would never get a fresh semantic vector. Callers probe
    /// first ([`Self::probe_semantic`]); the corrected result is written back
    /// into the shared cache.
    async fn embed_fresh(&self, text: &str) -> Result<(Vec<f32>, String), String> {
        let t = text.trim();
        if t.is_empty() {
            return Ok((vec![0.0f32; self.dim], HASH_EMBED_MODEL.into()));
        }
        let (vec, tag) = match self.embed_semantic(t).await {
            Some(v) => (v, self.model.clone()),
            None => (hash_embed(t, self.dim), HASH_EMBED_MODEL.into()),
        };
        self.state
            .lock()
            .unwrap()
            .cache
            .put(t.to_string(), vec.clone(), tag.clone());
        Ok((vec, tag))
    }

    fn model_tag(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_respects_cap_and_evicts_lru() {
        let mut c = EmbedCache::new(3);
        for i in 0..5 {
            c.put(format!("context {i}"), vec![i as f32], "m".into());
        }
        assert_eq!(c.len(), 3);
        // "context 4" was added last; earlier ones evicted.
        assert!(c.get("context 4").is_some());
        assert!(c.get("context 0").is_none());
    }

    #[test]
    fn cache_reorders_on_put() {
        let mut c = EmbedCache::new(2);
        c.put("a".into(), vec![1.0], "m".into());
        c.put("b".into(), vec![2.0], "m".into());
        c.put("a".into(), vec![1.0], "m".into());
        // order now b, a -> adding c evicts b
        c.put("c".into(), vec![3.0], "m".into());
        assert!(c.get("a").is_some());
        assert!(c.get("b").is_none());
    }

    #[test]
    fn cache_stores_the_space_tag_per_entry() {
        let mut c = EmbedCache::new(4);
        c.put(
            "t".into(),
            vec![1.0],
            "qwen3-embedding:0.6b".into(),
        );
        c.put(
            "u".into(),
            vec![2.0],
            HASH_EMBED_MODEL.into(),
        );
        assert_eq!(c.get("t").unwrap().1, "qwen3-embedding:0.6b");
        assert_eq!(c.get("u").unwrap().1, HASH_EMBED_MODEL);
    }
}
