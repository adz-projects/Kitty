//! In-process llama.cpp engine (docs/ANDROID.md §3, D1).
//!
//! Everything here is behind the `local-engine` cargo feature — see this
//! crate's `Cargo.toml` for why (`llama-cpp-sys-2` builds llama.cpp from
//! source via cmake, which ordinary daemon work shouldn't pay for).
//!
//! Deliberately additive: nothing in this module changes how the existing
//! HTTP providers behave. Phase 2a runs the local engine *alongside* Ollama so
//! the two can be compared on real hardware before Phase 2b retires the
//! managed Ollama process.
//!
//! Layout mirrors §3.1:
//! - [`engine`] — `LocalEngine`, the thin owned wrapper over a loaded model.
//! - [`manager`] — resident slot manager; who is loaded, and swapping.
//! - `provider` / `embeddings` / `summarizer` — consumers built on those.

pub mod embeddings;
pub mod engine;
pub mod manager;

pub use engine::{EmbedPooling, LocalEngine, LocalEngineError};
pub use manager::{SlotKind, SlotManager, SlotStatus};
