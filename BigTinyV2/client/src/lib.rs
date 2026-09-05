//! The BigTiny V2 client library.
//!
//! One implementation of "find the daemon, prove it is ours, register, talk to
//! it", shared by every frontend. V1 had no such crate: Kitty was the sole
//! client and kept a hand-rolled copy under `src-tauri/src/bigtiny/`. That was
//! fine for one app and becomes three subtly-different copies of SSE framing
//! for three.
//!
//! Start at [`discovery::attach_or_spawn`].

pub mod client;
pub mod discovery;
pub mod dispatcher;
pub mod error;
pub mod paths;
pub mod sse;

pub use client::{BigTinyClient, ProviderInfo};
pub use dispatcher::{Dispatcher, Job, JobOutcome};
pub use error::{ClientError, Result};

/// The wire-contract version this client requires.
///
/// Passed to [`discovery::DiscoveryConfig::min_api_version`] so attaching to an
/// older daemon fails loudly instead of sending request shapes it cannot parse.
pub const MIN_API_VERSION: u32 = bigtiny2_protocol::API_VERSION;
