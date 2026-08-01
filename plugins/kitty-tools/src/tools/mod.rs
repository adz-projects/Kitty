//! Plain-function tool bodies — `server.rs` holds the `#[tool]` stubs that
//! deserialize params and call into these; this is what makes conformance
//! tests possible (call the plain functions in-process, no MCP transport).

pub mod cache;
pub mod fs;
pub mod scratchpad;
pub mod shell;
pub mod viz;
pub mod workspace;

use std::path::PathBuf;

/// `~/.cache/lean-goose-mcp` — same directory the retired `replacement-mcp`
/// used, kept byte-identical so existing users' cached scrapes and
/// scratchpad data aren't orphaned by the port.
pub fn cache_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache")
        .join("lean-goose-mcp")
}
