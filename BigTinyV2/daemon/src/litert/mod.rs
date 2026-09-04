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

#[cfg(feature = "litert-embed")]
pub mod embedder;

#[cfg(feature = "litert-embed")]
pub use embedder::LiteRtEmbedder;

// Generative summarizer: LiteRT-LM, Windows-only (the crate is a Windows-only
// optional dep enabled by `litert-engine`).
#[cfg(all(windows, feature = "litert-engine"))]
pub mod summarizer;

#[cfg(all(windows, feature = "litert-engine"))]
pub use summarizer::LiteRtSummarizer;
