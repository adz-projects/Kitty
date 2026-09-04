//! Pure-Rust MCP (Model Context Protocol) client support.
//!
//! Ports `plugins/bigtiny/bigtiny/mcp/{manager.py,tools.py}`. Two of the three
//! transports (stdio, streamable_http) are spec-compliant and delegate to the
//! official `rmcp` crate; the third (`sse`) is BigTiny's own long-standing,
//! non-spec-compliant POST/JSON transport and is hand-rolled to match it
//! exactly — see `sse_transport.rs` for why `rmcp`'s SSE client would not
//! interoperate with servers built against this daemon's existing behavior.

pub mod builtin;
pub mod child_transport;
pub mod client;
pub mod manager;
pub mod rw_transport;
pub mod sse_transport;
pub mod tools;

pub use manager::MCPManager;
