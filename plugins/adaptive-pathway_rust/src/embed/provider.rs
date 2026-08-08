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
        if let Some((v, semantic)) = self.state.lock().unwrap().cache.get(t).cloned() {
            return (v, semantic);
        }
        let (vec, semantic) = match self.embed_ollama(t).await {
            Some(v) => (v, true),
            None => (hash_embed(t, self.dim), false),
        };
        self.state
            .lock()
            .unwrap()
            .cache
            .put(t.to_string(), vec.clone(), semantic);
        (vec, semantic)
    }

    /// Like `embed_with_space`, but bypasses the cache entirely (both read
    /// and write-through of a fresh result). Used by re-embed passes that
    /// have just confirmed Ollama is reachable (`probe_ollama`) and need a
    /// genuine retry attempt for `text` — a cache hit here would otherwise
    /// keep returning a hash-fallback vector cached during a *prior* outage
    /// forever, since nothing else ever invalidates or expires that entry,
    /// so the exact belief the re-embed pass exists to fix would never
    /// actually get a fresh semantic embedding.
    pub async fn embed_fresh_with_space(&self, text: &str) -> (Vec<f32>, bool) {
        let t = text.trim();
        if t.is_empty() {
            return (vec![0.0; self.dim], false);
        }
        let (vec, semantic) = match self.embed_ollama(t).await {
            Some(v) => (v, true),
            None => (hash_embed(t, self.dim), false),
        };
        // Refresh the shared cache with the up-to-date result so subsequent
        // `embed_with_space` calls for the same text also see the corrected
        // space instead of a stale entry.
        self.state
            .lock()
            .unwrap()
            .cache
            .put(t.to_string(), vec.clone(), semantic);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_for(url: String) -> EmbeddingProvider {
        let cfg = Config {
            embedding: crate::config::EmbeddingConfig {
                ollama_url: url,
                ..Default::default()
            },
            ..Default::default()
        };
        EmbeddingProvider::new(cfg)
    }

    /// Regression (88bugs #90): a cache hit must report the space that
    /// actually produced the cached vector, not a hardcoded `false`/`true` —
    /// otherwise a genuine Ollama embedding served from cache gets mistagged
    /// as lexical-hash, or (the original defect) a hash-fallback vector
    /// cached during an outage gets read back as "unknown"/never-semantic
    /// forever even once Ollama recovers and the real vector is available.
    #[tokio::test]
    async fn cache_hit_reports_the_space_that_actually_produced_it() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/embeddings")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"embedding": [0.1, 0.2, 0.3]}"#)
            .expect(1)
            .create_async()
            .await;
        let provider = provider_for(server.url());

        let (v1, semantic1) = provider.embed_with_space("hello world").await;
        assert!(semantic1, "first call must hit the (mocked) Ollama endpoint");

        // Second call for the exact same text must be served from cache,
        // WITHOUT a second HTTP request (`mock.expect(1)` above is the real
        // assertion), and must still report `semantic == true`.
        let (v2, semantic2) = provider.embed_with_space("hello world").await;
        assert!(semantic2, "a cached semantic vector must still read back as semantic");
        assert_eq!(v1, v2);
        mock.assert_async().await;
    }

    /// The `embed_fresh_with_space` bypass used by `reembed_stale_beliefs`
    /// must not be satisfied by a stale cache entry — it exists precisely
    /// because a normal `embed_with_space` cache hit would otherwise keep
    /// returning a hash-fallback vector cached during a *prior* outage
    /// forever, even after Ollama is confirmed back up and a real semantic
    /// embedding is available for the exact same text.
    #[tokio::test]
    async fn embed_fresh_bypasses_a_stale_cached_hash_vector() {
        // A single stateful mock simulating an outage that later recovers:
        // the first request fails (no usable `embedding` field), every
        // request after that succeeds. Using one mock with a request-counted
        // callback (rather than two separately-registered mocks matching the
        // same path) avoids relying on mockito's mock-precedence rules for
        // "which of two identical-path mocks wins".
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_cb = hits.clone();
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/api/embeddings")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |_req| {
                if hits_cb.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    b"{}".to_vec()
                } else {
                    br#"{"embedding": [0.1, 0.2, 0.3]}"#.to_vec()
                }
            })
            .expect_at_least(1)
            .create_async()
            .await;
        let provider = provider_for(server.url());

        let (_hash_vec, semantic) = provider.embed_with_space("hello world").await;
        assert!(!semantic, "the first (failing) response must fall back to the hash embedder");
        assert_eq!(provider.cache_len(), 1);

        // Ollama "recovers" (every request from here on succeeds). A plain
        // cache-checking call still returns the stale hash vector, tagged
        // non-semantic — this is the defect `embed_fresh_with_space` exists
        // to route around, not something a plain cache hit can fix on its
        // own.
        let (_stale, still_cached_as_hash) = provider.embed_with_space("hello world").await;
        assert!(!still_cached_as_hash, "a plain cache hit is expected to still surface the stale entry");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1, "a cache hit must not perform a new HTTP request");

        // `reembed_stale_beliefs` always probes before re-embedding —
        // mirror that here, since it's what actually clears the
        // `available == Some(false)` + probe-interval gate that would
        // otherwise short-circuit `embed_ollama` before it ever reaches the
        // network, independent of the cache.
        assert!(provider.probe_ollama().await, "the probe must see the recovered endpoint");
        let hits_after_probe = hits.load(std::sync::atomic::Ordering::SeqCst);

        // The fresh/bypass path must ignore that cache entry and actually
        // retry the (now-healthy) endpoint.
        let (_fresh_vec, semantic_after_recovery) =
            provider.embed_fresh_with_space("hello world").await;
        assert!(
            semantic_after_recovery,
            "embed_fresh_with_space must retry Ollama instead of trusting the poisoned cache entry"
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            hits_after_probe + 1,
            "embed_fresh_with_space must issue a real request"
        );

        // And it refreshes the shared cache, so subsequent plain lookups for
        // the same text now correctly report semantic too.
        let (_refreshed, now_semantic) = provider.embed_with_space("hello world").await;
        assert!(now_semantic, "embed_fresh_with_space must write its corrected result back into the cache");
    }
}
