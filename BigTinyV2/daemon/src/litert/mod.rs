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

/// Embedding requests allowed to wait for an embedder's single actor thread.
///
/// Several apps' recalls, ingests and learn passes all feed one thread, and
/// the queue used to be unbounded: when they outran it, the backlog of
/// request texts grew without limit while each caller pinned a blocking-pool
/// thread to wait. Past this depth a request is answered "unavailable" at
/// once, which every caller already handles with its lexical fallback.
#[cfg(feature = "litert-embed")]
const EMBED_QUEUE_DEPTH: usize = 128;

/// Queue `cmd` for an embedder actor, or `None` when the queue is full or the
/// actor is gone.
#[cfg(feature = "litert-embed")]
fn enqueue<T>(tx: &std::sync::Mutex<std::sync::mpsc::SyncSender<T>>, cmd: T) -> Option<()> {
    use std::sync::mpsc::TrySendError;
    match tx.lock().ok()?.try_send(cmd) {
        Ok(()) => Some(()),
        Err(TrySendError::Full(_)) => {
            tracing::warn!("embedder queue full ({EMBED_QUEUE_DEPTH}); answering unavailable");
            None
        }
        Err(TrySendError::Disconnected(_)) => None,
    }
}

#[cfg(all(test, feature = "litert-embed"))]
mod tests {
    use super::*;

    #[test]
    fn a_full_queue_refuses_instead_of_growing() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<usize>(EMBED_QUEUE_DEPTH);
        let tx = std::sync::Mutex::new(tx);
        for i in 0..EMBED_QUEUE_DEPTH {
            assert!(enqueue(&tx, i).is_some());
        }
        assert!(enqueue(&tx, 999).is_none(), "the queue must stay bounded");
        drop(rx);
        let (tx2, rx2) = std::sync::mpsc::sync_channel::<usize>(1);
        drop(rx2);
        assert!(enqueue(&std::sync::Mutex::new(tx2), 1).is_none(), "a gone actor is unavailable");
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
