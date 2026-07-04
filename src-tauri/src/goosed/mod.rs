//! ACP client for goosed (`goose serve`). All ACP method names / transport
//! details live here — the isolation boundary from CLAUDE.md. Targets Goose
//! 1.41.0 ACP-over-WebSocket; see docs/acp-protocol.md.

pub mod api;
pub mod stream;
