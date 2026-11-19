//! In-process **LiteRT** engine — the successor to the llama.cpp `local`
//! module (see the repo plan "Replace llama.cpp with LiteRT on both platforms").
//!
//! Two roles, both proven on Windows + Android during the Phase 0 spike:
//! - [`embedder`] — EmbeddingGemma `.tflite` semantic embeddings, the
//!   cross-platform half. Runs everywhere via `edgefirst-tflite` loading the
//!   real `libLiteRt.{dll,so}` at runtime (`Library::from_path`), tokenizing
//!   with the pure-Rust `tokenizers` crate. Feature `litert-embed`.
//! - `summarizer` — generative compaction on Windows only, via LiteRT-LM
//!   (`gemma-4-E2B-it.litertlm`). Feature `litert-engine` (adds `litert-lm-rust`,
//!   Windows-only). Android offloads compaction to the remote chat model, so no
//!   generative model runs on the phone. (Landed in a later step.)
//!
//! The native runtime library is **not** built from source (unlike llama.cpp):
//! it is a prebuilt shipped beside the daemon (Windows) or in the APK `jniLibs`
//! (Android), and loaded by path. That is what removes the entire cmake / NDK /
//! Vulkan build surface `local-engine` carried.

use std::sync::Arc;

use crate::embed_priority::{PriorityQueue, QueueDepth, Refused};

/// The queue between an embedder's callers and its one actor thread.
///
/// Several apps' recalls, ingests and learn passes all feed that thread, and
/// the queue used to be unbounded: when they outran it, the backlog of
/// request texts grew without limit while each caller pinned a blocking-pool
/// thread to wait. It is now bounded (`embed_priority::QUEUE_DEPTH`) and
/// two-laned, so a live recall goes first and is never refused because of a
/// background backlog.
fn new_queue<T: Send + 'static>() -> Arc<PriorityQueue<T>> {
    let queue = Arc::new(PriorityQueue::new(crate::embed_priority::QUEUE_DEPTH));
    let probe: Arc<dyn QueueDepth> = queue.clone();
    crate::embed_priority::register(&probe);
    queue
}

/// Queue `cmd` for an embedder actor, at the calling task's priority, or
/// `None` when it was refused -- which every caller already handles as
/// "embedder unavailable" (lexical fallback).
fn enqueue<T>(queue: &PriorityQueue<T>, cmd: T) -> Option<()> {
    match queue.push(cmd, crate::embed_priority::is_interactive()) {
        Ok(displaced) => {
            if displaced.is_some() {
                // Dropping it drops its reply sender: that caller gets "unavailable".
                tracing::warn!("embedder queue full; a background request yielded to a live recall");
            }
            Some(())
        }
        Err(Refused::Full) => {
            tracing::warn!("embedder queue full; answering unavailable");
            None
        }
        Err(Refused::Closed) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_queue_refuses_instead_of_growing() {
        let q = new_queue::<usize>();
        for i in 0..crate::embed_priority::QUEUE_DEPTH {
            assert!(enqueue(&q, i).is_some());
        }
        assert!(enqueue(&q, 999).is_none(), "the queue must stay bounded");
        q.close();
        assert!(enqueue(&q, 1).is_none(), "a gone actor is unavailable");
    }

    #[tokio::test]
    async fn a_live_recall_is_queued_even_when_background_work_has_filled_it() {
        let q = new_queue::<usize>();
        for i in 0..crate::embed_priority::QUEUE_DEPTH {
            enqueue(&q, i).unwrap();
        }
        let ok = crate::embed_priority::interactive(async { enqueue(&q, 999) }).await;
        assert!(ok.is_some());
        assert_eq!(q.pop(), Some(999), "and it is served first");
    }
}

#[cfg(feature = "litert-embed")]
pub mod embedder;

#[cfg(feature = "litert-embed")]
pub use embedder::LiteRtEmbedder;

// EmbeddingGemma 2 `.litertlm` bundles through LiteRT-LM's embedding API
// (both platforms; the runtime library is opened at run time).
#[cfg(feature = "litert-embed")]
pub mod lm_embedder;

#[cfg(feature = "litert-embed")]
pub use lm_embedder::LiteRtLmEmbedder;

// Generative summarizer: LiteRT-LM, Windows-only (the crate is a Windows-only
// optional dep enabled by `litert-engine`).
#[cfg(all(windows, feature = "litert-engine"))]
pub mod summarizer;

#[cfg(all(windows, feature = "litert-engine"))]
pub use summarizer::LiteRtSummarizer;
