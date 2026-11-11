//! One cache in front of the engine's shared embedding model.
//!
//! Adaptive Pathway and Memorabilia both embed the turn's user message (and
//! the previous one) when a turn starts, and their recalls run together, so
//! the same text used to go through the one local model thread twice, back to
//! back. Each engine keeps its own cache, but neither sees the other's, and
//! two concurrent misses both compute. This sits below both: one result per
//! text, and a request for a text already being embedded waits for that call
//! instead of making another.

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use tokio::sync::OnceCell;

use adaptive_pathway::embed::SemanticEmbedder;

/// Texts kept. A turn needs two; a few turns' worth covers follow-ups and
/// the counterpoint lookup without growing.
const CAPACITY: usize = 64;

pub struct SharedEmbedCache {
    inner: Arc<dyn SemanticEmbedder>,
    entries: DashMap<String, Arc<OnceCell<Option<Vec<f32>>>>>,
}

impl SharedEmbedCache {
    pub fn new(inner: Arc<dyn SemanticEmbedder>) -> Self {
        Self {
            inner,
            entries: DashMap::new(),
        }
    }
}

#[async_trait]
impl SemanticEmbedder for SharedEmbedCache {
    async fn embed(&self, text: &str) -> Option<Vec<f32>> {
        if self.entries.len() >= CAPACITY && !self.entries.contains_key(text) {
            self.entries.clear();
        }
        let cell = self
            .entries
            .entry(text.to_string())
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone();
        let v = cell
            .get_or_init(|| async { self.inner.embed(text).await })
            .await
            .clone();
        // A failure is not kept: the next request tries the model again.
        if v.is_none() {
            self.entries.remove(text);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counting(AtomicUsize);

    #[async_trait]
    impl SemanticEmbedder for Counting {
        async fn embed(&self, text: &str) -> Option<Vec<f32>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Some(vec![text.len() as f32])
        }
    }

    #[tokio::test]
    async fn concurrent_requests_for_one_text_share_one_call() {
        let inner = Arc::new(Counting(AtomicUsize::new(0)));
        let cache = SharedEmbedCache::new(inner.clone());
        let (a, b) = tokio::join!(cache.embed("hello"), cache.embed("hello"));
        assert_eq!(a, b);
        assert_eq!(cache.embed("hello").await, a);
        assert_eq!(inner.0.load(Ordering::SeqCst), 1);
        cache.embed("other").await;
        assert_eq!(inner.0.load(Ordering::SeqCst), 2);
    }
}
