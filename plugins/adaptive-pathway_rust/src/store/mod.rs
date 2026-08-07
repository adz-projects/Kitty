//! Persistent storage for the behavioral-memory engine. Own `pathway.db`,
//! in the same directory as `bigtiny.db`, with its own `sqlx::migrate!`
//! chain. PRAGMAs match the daemon: WAL, synchronous=NORMAL, foreign_keys,
//! busy_timeout.

pub mod assumptions;
pub mod audit;
pub mod beliefs;
pub mod contradictions;
pub mod conversation;
pub mod domains;
pub mod observations;
pub mod suppressions;

use std::path::Path;
use std::str::FromStr;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::SqlitePool;

use crate::error::{PathwayError, Result};

pub struct Db {
    pool: SqlitePool,
}

impl Db {
    /// Open (creating if needed) the DB at `path`, apply PRAGMAs + migrations.
    pub async fn open(path: &str) -> Result<Self> {
        if !path.starts_with("sqlite:") && !path.contains("::memory:") {
            if let Some(parent) = Path::new(path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).map_err(PathwayError::Io)?;
                }
            }
        }
        let options = if path.starts_with("sqlite:") || path.contains("::memory:") {
            SqliteConnectOptions::from_str(path)
                .map_err(|e| PathwayError::Config(e.to_string()))?
        } else {
            SqliteConnectOptions::new().filename(path).create_if_missing(true)
        };
        let pool = SqlitePool::connect_with(options).await?;
        Self::init(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn open_in_memory() -> Result<Self> {
        Self::open("sqlite::memory:").await
    }

    async fn init(pool: &SqlitePool) -> Result<()> {
        sqlx::query("PRAGMA journal_mode = WAL").execute(pool).await?;
        sqlx::query("PRAGMA synchronous = NORMAL").execute(pool).await?;
        sqlx::query("PRAGMA foreign_keys = ON").execute(pool).await?;
        sqlx::query("PRAGMA busy_timeout = 5000").execute(pool).await?;
        sqlx::migrate!("./migrations").run(pool).await.map_err(|e| {
            PathwayError::Migrate(e.to_string())
        })?;
        Ok(())
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

/// Encode an `f32` slice as a BLOB (little-endian bytes), for the
/// `embedding BLOB` columns.
pub fn encode_embedding(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Decode a BLOB back into `&[f32]`. Falls back to empty on wrong length.
pub fn decode_embedding(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}
