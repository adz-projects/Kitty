// The document library (Word read/write, PDF/Excel/text extraction, document
// cache, path/config plumbing) lives in `kitty-doc` so apps without the MCP
// server can link it alone. Re-exported at the original paths: every
// `kitty_tools::…` call site, including the daemon's, is unchanged.
pub use kitty_doc::{config, doc_store, docx, envelope, extract, paths, query_filter, text};

pub mod server;
pub mod tools;

use rmcp::ServiceExt;
use tokio::io::{AsyncRead, AsyncWrite};

pub use config::InProcessConfig;
use server::KittyToolsServer;

/// Runs a fresh `KittyToolsServer` over any duplex byte stream until the
/// stream closes — the entry point for a host that links this crate
/// in-process instead of spawning `main.rs`'s stdio binary (an
/// exec()-restricted platform, e.g. Android, where the frozen
/// `kitty-tools.exe`/ELF this ships as on desktop can't be launched as a
/// child process). `main.rs`'s `rmcp::transport::stdio()` and a stream this
/// function is handed both resolve to the same underlying transport impl
/// (any `AsyncRead + AsyncWrite`), so this is the whole difference between
/// the two hosting modes — same server, same tool router, same behavior.
///
/// `config` is what the stdio binary would have read from its environment;
/// see [`config`] for why an in-process host must pass it explicitly.
pub async fn serve_in_process<S>(stream: S, config: InProcessConfig) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let server = KittyToolsServer::with_config(config).serve(stream).await?;
    server.waiting().await?;
    Ok(())
}
