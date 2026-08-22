pub mod doc_store;
pub mod docx;
pub mod envelope;
pub mod paths;
pub mod query_filter;
pub mod server;
pub mod text;
pub mod tools;

use rmcp::ServiceExt;
use tokio::io::{AsyncRead, AsyncWrite};

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
pub async fn serve_in_process<S>(stream: S) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let server = KittyToolsServer::new().serve(stream).await?;
    server.waiting().await?;
    Ok(())
}
