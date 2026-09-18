//! Embedding layer (plan §3.3 / §15 Phase 1): the deterministic lexical
//! signed-hash fallback, wrap-add dimension projection, and the Ollama
//! semantic provider with timeout→fallback, circuit-breaker availability,
//! and the LRU text cache. Vectors from the two spaces never mix — every
//! result carries the tag of the space that produced it.

pub mod hashing;
pub mod project;
pub mod provider;

pub use hashing::{hash_embed, word_tokens, HashEmbedder};
pub use project::project;
pub use provider::{EmbedCache, OllamaEmbedder};
