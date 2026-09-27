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
//! client misread a response, and when a release adds routes a client built
//! against it may depend on -- so such a client refuses an older daemon with
//! a clear "update the other app" error instead of failing call by call. It
//! is not bumped for additive fields, which serde already tolerates in both
//! directions.

pub mod discovery;
pub mod events;

pub use events::{serialize_sse, SSEEvent, SSEEventType};

/// The wire-contract version this build speaks.
///
/// A client declares the minimum it can work with; a daemon advertises what it
/// serves (in the handshake file and on `GET /api/health`). Attaching to a
/// daemon older than the client needs must surface a clear error rather than a
/// silently wrong-shaped call -- see `discovery::Handshake::is_compatible_with`.
///
/// * 1 -- BigTiny 2.0.
/// * 2 -- BigTiny 2.1: admin restart, app reclaim, the app event stream,
///   approval-rule management, schedules v2, memory erase, V1 merge import,
///   app purge. Every v1 route is unchanged, so v1 clients are served as
///   before.
pub const API_VERSION: u32 = 2;
