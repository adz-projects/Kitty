//! Plain-function tool bodies — `server.rs` holds the `#[tool]` stubs that
//! deserialize params and call into these; this is what makes conformance
//! tests possible (call the plain functions in-process, no MCP transport).

pub mod cache;
pub mod scratchpad;
pub mod shell;
pub mod shell_ro;
pub mod viz;
pub mod workspace;

// The document tool bodies live in kitty-doc; re-exported at their original
// paths so server.rs and the conformance tests are unchanged.
pub use kitty_doc::tools::{excel, fs, image, pdf};

// One cache directory for both crates: kitty-doc's document cache and this
// crate's scrape/scratchpad tools must agree on where it is.
pub use kitty_doc::tools::cache_dir;
