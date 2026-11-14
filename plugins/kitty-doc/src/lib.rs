//! Document library: Word read/write, PDF/Excel/text extraction and the
//! extract-once document cache, split out of `kitty-tools` so apps that need
//! the document code without the MCP server (Bacon) can link it directly.
//!
//! `kitty-tools` re-exports every module here at its original path, so its
//! public API and tool behavior are unchanged. The module tree is kept
//! identical on purpose: internal `crate::` references resolve the same way
//! in both crates.
//!
//! The envelope-free entry points for library callers are
//! [`extract::extract_document_text`] and [`docx::write::write_document`];
//! neither does path-allow gating (the caller owns the paths it passes).

pub mod config;
pub mod doc_store;
pub mod docx;
pub mod envelope;
pub mod extract;
pub mod paths;
#[cfg(feature = "pdf-render")]
pub mod pdf_render;
pub mod query_filter;
pub mod text;
pub mod tools;
