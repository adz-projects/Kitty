//! The SSE wire types. Defined once, in `bigtiny2-protocol`, and re-exported
//! here so the daemon's existing `crate::server::events::*` paths keep working.
//!
//! This used to be a second, hand-maintained copy of the same file, and the
//! copies had already drifted: the daemon's lacked the `#[serde(default)]`s,
//! so an event it serialized with a field omitted could not be deserialized
//! again by its own type. One definition makes that class of drift impossible.

pub use bigtiny2_protocol::events::*;
