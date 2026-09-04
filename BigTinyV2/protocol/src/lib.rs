//! The BigTiny V2 wire contract.
//!
//! Every type that crosses the HTTP boundary lives here, so the daemon and any
//! client are compiled against one definition rather than two that drift. This
//! is the crate a consumer depends on to *name* things; [`bigtiny2_client`] is
//! the one that talks to the daemon.
//!
//! ## Versioning
//!
//! [`API_VERSION`] is the single number a client checks on attach (see the
//! handshake in `discovery`). It is bumped when a change would make an older
//! client misread a response -- not for additive fields, which serde already
//! tolerates in both directions.

pub mod discovery;
pub mod events;

pub use events::{serialize_sse, SSEEvent, SSEEventType};

/// The wire-contract version this build speaks.
///
/// A client declares the minimum it can work with; a daemon advertises what it
/// serves (in the handshake file and on `GET /api/health`). Attaching to a
/// daemon older than the client needs must surface a clear error rather than a
/// silently wrong-shaped call -- see `discovery::Handshake::is_compatible_with`.
pub const API_VERSION: u32 = 1;
