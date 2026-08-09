//! `EmbeddingProvider`: turns arbitrary context text into a fixed-dimension
//! vector. Tries a *semantic* embedder first, falling back to the
//! deterministic signed-hashing vectorizer (lexical) when that is unavailable
//! or errors. Ported from `embeddings.py::EmbeddingProvider`.
//!
//! There are two semantic backends:
//!
//! - **In-process** ([`SemanticEmbedder`], injected by the host). This is what
//!   BigTiny uses: the engine is linked into the same binary, so going out
//!   over HTTP to reach it would mean the daemon calling its own socket. See
//!   docs/ANDROID.md §10 Phase 2b.
//! - **Ollama over HTTP**, the original path, kept for anyone pointing at a
//!   real Ollama server (`AP_EMBED_OLLAMA_URL`).
//!
//! Both go through [`EmbeddingProvider::embed_semantic`], so the availability
//! circuit-breaker, dimension projection and cache behave identically whether
//! the failure was a dead socket or a model that wouldn't load.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::config::Config;

use super::hashing::{hash_embed, EmbedCache};
use super::project::project;

/// A host-supplied semantic embedder, called in-process instead of over HTTP.
///
/// Returns `None` for any failure — an unloadable model, a decode error, an
/// empty result. The provider treats that exactly like an unreachable Ollama:
/// trip the circuit-breaker and fall back to lexical hashing.
#[async_trait]
pub trait SemanticEmbedder: Send + Sync {
    /// Embed `text`, returning the model's native-width vector. The caller
    /// projects it to the configured dimension — don't do that here.
    async fn embed(&self, text: &str) -> Option<Vec<f32>>;
}

pub struct EmbeddingProvider {
    dim: usize,
    ollama_url: String,
    ollama_model: String,
    timeout: Duration,
    probe_interval: Duration,
    client: reqwest::Client,
    /// When set, replaces the HTTP path entirely.
    embedder: Option<Arc<dyn SemanticEmbedder>>,
    state: Arc<Mutex<ProviderState>>,
}

struct ProviderState {
    available: Option<bool>, // None = never probed; Some(true/false)
    last_probe: Instant,
    cache: EmbedCache,
}

impl EmbeddingProvider {
    pub fn new(cfg: Config) -> Self {
        Self::with_embedder(cfg, None)
    }

    /// Construct with a host-supplied in-process embedder. `None` keeps the
    /// HTTP-to-Ollama behaviour.
    pub fn with_embedder(cfg: Config, embedder: Option<Arc<dyn SemanticEmbedder>>) -> Self {
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
            embedder,
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
    /// semantic embedder (`true`) or the lexical signed-hash fallback
    /// (`false`). Callers that persist embeddings (learn/background/mcp) MUST
    /// tag the stored `embedding_model` with the space actually used — tagging
    /// a hash-space vector as the configured semantic model puts it in the same
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
        let (vec, semantic) = match self.embed_semantic(t).await {
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
    /// have just confirmed the semantic embedder is up (`probe_semantic`) and need a
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
        let (vec, semantic) = match self.embed_semantic(t).await {
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

    /// The one semantic path: circuit-breaker, backend dispatch, projection,
    /// availability bookkeeping. Both callers of the semantic embedder go
    /// through here so an in-process failure and a dead Ollama are handled
    /// identically.
    async fn embed_semantic(&self, text: &str) -> Option<Vec<f32>> {
        {
            let st = self.state.lock().unwrap();
            match st.available {
                Some(false) if st.last_probe.elapsed() < self.probe_interval => return None,
                _ => {}
            }
        }
        let raw = match &self.embedder {
            Some(e) => e.embed(text).await,
            None => self.fetch_ollama(text).await,
        };
        match raw {
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

    /// Force a fresh availability probe; returns whether the semantic embedder
    /// answered — whichever backend is configured.
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

    /// An injected embedder must replace the HTTP path outright — not race it,
    /// not fall back to it. The mock server here is registered with
    /// `expect(0)`: if the provider ever reaches for HTTP while an in-process
    /// embedder is present, that assertion fails. This is the property the
    /// whole in-process swap rests on (docs/ANDROID.md §10 Phase 2b): the
    /// daemon must never call its own socket to embed.
    #[tokio::test]
    async fn an_injected_embedder_replaces_the_http_path() {
        struct Fixed(Vec<f32>);
        #[async_trait]
        impl SemanticEmbedder for Fixed {
            async fn embed(&self, _text: &str) -> Option<Vec<f32>> {
                Some(self.0.clone())
            }
        }

        let mut server = mockito::Server::new_async().await;
        let never = server
            .mock("POST", "/api/embeddings")
            .expect(0)
            .create_async()
            .await;

        let cfg = Config {
            embedding: crate::config::EmbeddingConfig {
                ollama_url: server.url(),
                ..Default::default()
            },
            ..Default::default()
        };
        let provider =
            EmbeddingProvider::with_embedder(cfg, Some(Arc::new(Fixed(vec![0.1, 0.2, 0.3]))));

        let (v, semantic) = provider.embed_with_space("hello world").await;
        assert!(semantic, "an in-process embedder produces the semantic space");
        assert!(v.iter().any(|x| *x != 0.0), "got a zero vector");
        assert!(provider.probe_semantic().await);
        never.assert_async().await;
    }

    /// A failing in-process embedder must degrade exactly like an unreachable
    /// Ollama: lexical hash space, tagged non-semantic. A model that won't
    /// load must lower recall quality, never take the turn down.
    #[tokio::test]
    async fn a_failing_injected_embedder_falls_back_to_hash_space() {
        struct Broken;
        #[async_trait]
        impl SemanticEmbedder for Broken {
            async fn embed(&self, _text: &str) -> Option<Vec<f32>> {
                None
            }
        }
        let provider = EmbeddingProvider::with_embedder(Config::default(), Some(Arc::new(Broken)));
        let (v, semantic) = provider.embed_with_space("hello world").await;
        assert!(!semantic);
        assert_eq!(v.len(), Config::default().embedding_dim);
        assert!(!provider.probe_semantic().await);
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
        // otherwise short-circuit `embed_semantic` before it ever reaches the
        // network, independent of the cache.
        assert!(provider.probe_semantic().await, "the probe must see the recovered endpoint");
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
