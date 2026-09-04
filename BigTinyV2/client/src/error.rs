//! Client errors.
//!
//! Two of these carry meaning a caller must be able to act on, so they are
//! distinct variants rather than a single opaque string:
//!
//!   * [`ClientError::DaemonTooOld`] -- another app is running an older
//!     BigTiny. The user has to update that app; retrying will never help, and
//!     proceeding anyway would send request shapes the daemon cannot parse.
//!   * [`ClientError::AlreadyRegistered`] -- this `app_id` exists but we do not
//!     hold its key. Almost always a lost secret store, not a bug, and the
//!     caller's remedy (re-register under a fresh id, or recover the key)
//!     depends on the app.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("no BigTiny daemon found and none could be started: {0}")]
    NotFound(String),

    #[error(
        "the running BigTiny daemon speaks API v{found}, but this app needs at least v{needed} \
         — another app is running an older BigTiny; update it"
    )]
    DaemonTooOld { found: u32, needed: u32 },

    #[error("app id {0:?} is already registered and this app does not hold its key")]
    AlreadyRegistered(String),

    #[error("could not take the spawn lock at {path:?}: {reason}")]
    SpawnLock { path: PathBuf, reason: String },

    #[error("daemon did not become healthy within {0:?}")]
    ReadinessTimeout(std::time::Duration),

    #[error("unauthorized — the stored app key was rejected")]
    Unauthorized,

    #[error("daemon returned {status}: {body}")]
    Http { status: u16, body: String },

    #[error(transparent)]
    Transport(#[from] reqwest::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("malformed response: {0}")]
    Decode(String),
}

pub type Result<T> = std::result::Result<T, ClientError>;
