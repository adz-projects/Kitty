//! Plain-function tool bodies — `server.rs` holds the `#[tool]` stubs that
//! deserialize params and call into these; this is what makes conformance
//! tests possible (call the plain functions in-process, no MCP transport).

pub mod cache;
pub mod excel;
pub mod fs;
pub mod pdf;
pub mod scratchpad;
pub mod shell;
pub mod viz;
pub mod workspace;

use std::path::PathBuf;

/// `~/.cache/lean-goose-mcp` — same directory the retired `replacement-mcp`
/// used, kept byte-identical so existing users' cached scrapes and
/// scratchpad data aren't orphaned by the port.
///
/// Resolved through `paths::home_dir`, not `dirs::home_dir` directly, for two
/// reasons. It picks up the `KITTY_PLUGIN_HOME` override, which is the only
/// thing that makes this directory writable on Android. And it agrees with
/// the boundary check: `paths::home_dir` prefers `%USERPROFILE%`/`$HOME` over
/// `dirs`, so when those disagreed, `cache::ensure_within_home` rejected this
/// crate's *own* cache directory as being outside home.
///
/// A `None` home yields a relative placeholder. That is not a usable cache
/// location, and deliberately so: every tool that touches this path runs it
/// through `path_within_home` first, which rejects when home is
/// undeterminable, so the tools fail cleanly instead of writing somewhere
/// arbitrary.
pub fn cache_dir() -> PathBuf {
    crate::paths::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache")
        .join("lean-goose-mcp")
}
