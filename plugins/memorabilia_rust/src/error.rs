//! Engine error type. Callers treat every variant as "this pass is
//! skipped" — errors inside memory handling are `tracing::warn`-logged and
//! swallowed, never returned as a hard failure that breaks the caller's
//! turn (claude.md, core principle 9).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(String),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("embedding error: {0}")]
    Embed(String),
    #[error("chat completion error: {0}")]
    Chat(String),
    #[error("extraction error: {0}")]
    Extract(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, Error>;
