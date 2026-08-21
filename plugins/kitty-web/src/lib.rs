//! `kitty-web` — web search and page scraping as a stdio (or in-process) MCP
//! server.
//!
//! The Rust replacement for the three web tools that used to live in the
//! Python `kitty-docs-web` plugin (`lean_web_search`,
//! `lean_web_search_read_chunk`, `lean_web_scrape`). Those are the tools that
//! matter most on a phone and the ones with clean Rust equivalents; the PDF
//! and Excel halves of `kitty-docs-web` stay Python for now (PyMuPDF and
//! openpyxl have no adequate Rust equivalent — see `docs/PLUGINS.md`).
//!
//! Tool names and response envelopes are deliberately unchanged from the
//! Python originals, so this is a drop-in swap from the model's point of
//! view — see `server.rs` and `envelope.rs` for why both are load-bearing.

pub mod envelope;
pub mod paths;
pub mod query_filter;
pub mod scrape;
pub mod search;
pub mod server;
pub mod ssrf;

use rmcp::ServiceExt;
use tokio::io::{AsyncRead, AsyncWrite};

use server::KittyWebServer;

/// Runs a fresh `KittyWebServer` over any duplex byte stream until the stream
/// closes — the entry point for a host that links this crate in-process
/// instead of spawning `main.rs`'s stdio binary (an exec()-restricted
/// platform, e.g. Android). Mirrors `kitty_tools::serve_in_process`; see
/// `docs/PLUGINS.md`'s "in-process MCP server" section.
pub async fn serve_in_process<S>(stream: S) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let server = KittyWebServer::new().serve(stream).await?;
    server.waiting().await?;
    Ok(())
}
