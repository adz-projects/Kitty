use thiserror::Error;

#[derive(Debug, Error)]
pub enum PathwayError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(String),
    #[error("embedding error: {0}")]
    Embed(String),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("recall error: {0}")]
    Recall(String),
    #[error("extraction error: {0}")]
    Extract(String),
    #[error("model error: {0}")]
    Model(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, PathwayError>;
