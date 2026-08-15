//! `kitty-wasm` — a sandboxed WebAssembly compute environment as an MCP
//! server.
//!
//! Replaces the `wasm-math-mcp` Python plugin, which despite its name was
//! neither WebAssembly nor a set of math tools: it exposed exactly one tool,
//! `execute_math_python`, running arbitrary Python in a `multiprocessing`
//! worker behind an AST allowlist. This crate is what that name always
//! described — a real WebAssembly sandbox (wasmtime + WASI) that a model can
//! execute code in for general work: calculation, data transformation,
//! parsing, analysis, and file processing against a scoped workspace.
//!
//! The security model is genuinely different, not just reimplemented. The
//! Python version inspected source for forbidden constructs — a denylist over
//! a dynamic language. Here the guest has no capability it isn't handed:
//! no network, no filesystem beyond explicit mounts, and runtime-enforced
//! ceilings on CPU time and memory. `sandbox.rs` documents the full policy.
//!
//! Layout:
//! - [`sandbox`] — the wasmtime engine, capability policy, and module cache
//! - [`capture`] — memory-bounded stdout/stderr (the `SmartStdoutBuffer` port)
//! - [`guest`] — resolving/verifying/installing the pinned CPython guest
//! - [`paths`] — home-directory containment for the `workspace` mount
//! - [`python`] — the Python harness and its response envelope
//! - [`server`] — the MCP tool surface

pub mod capture;
pub mod guest;
pub mod paths;
pub mod python;
pub mod sandbox;
pub mod server;

use rmcp::ServiceExt;
use tokio::io::{AsyncRead, AsyncWrite};

use server::KittyWasmServer;

/// Runs a fresh `KittyWasmServer` over any duplex byte stream until it
/// closes — the entry point for a host that links this crate in-process
/// rather than spawning `main.rs`'s stdio binary. Mirrors
/// `kitty_tools::serve_in_process`; see `docs/PLUGINS.md`.
pub async fn serve_in_process<S>(stream: S) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let server = KittyWasmServer::new().serve(stream).await?;
    server.waiting().await?;
    Ok(())
}
