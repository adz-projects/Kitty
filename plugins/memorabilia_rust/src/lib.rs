//! Memorabilia — Declarative Factual Memory System.
//!
//! Single-user, dynamic working memory: ingests evidence (Level 0 chunks),
//! abstracts atomic claims (Level 1 propositions), tracks temporal deadlines,
//! and serves a token-capped factual subgraph to a calling LLM at query time.
//!
//! Dependency direction is inverted (project-plan.md §13.1): this crate
//! depends only on trait seams (`StructuredChat`, `Embedder`, `VectorIndex`)
//! — never on concrete providers — so the same engine runs under tests
//! (`MockChat` + deterministic hash embedder), the daemon, and any future
//! host. Every public entry point soft-fails: errors are logged and
//! swallowed, and with nothing to say the payload is byte-identical to
//! no-memory behavior (claude.md, core principle 9).
//!
//! The full specification lives in `project-plan.md`; the working invariants
//! in `claude.md`.

pub mod config;
pub mod core;
pub mod embed;
pub mod engine;
pub mod error;
pub mod learn;
pub mod maintenance;
pub mod mcp;
pub mod privacy;
pub mod recall;
pub mod reliability;
pub mod store;
pub mod text;
pub mod traits;
