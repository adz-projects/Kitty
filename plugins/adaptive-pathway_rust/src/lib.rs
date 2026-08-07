//! Adaptive Pathway → Behavioral Memory (Rust rewrite).
//!
//! Models who the user is, what they care about, and how the assistant
//! should adapt. Linked in-process into the BigTiny daemon. See
//! `plugins/adaptive-pathway/docs/adaptive-pathway-v2.md`.

pub mod antisycophancy;
pub mod belief;
pub mod config;
pub mod domains;
pub mod embed;
pub mod engine;
pub mod error;
pub mod layers;
pub mod recall;
pub mod store;
pub mod vector;
