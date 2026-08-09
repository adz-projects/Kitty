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
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;

use crate::error::{PathwayError, Result};

pub struct Db {
    pool: SqlitePool,
}

impl Db {
    /// Open (creating if needed) the DB at `path`, apply PRAGMAs + migrations.
    pub async fn open(path: &str) -> Result<Self> {
        let is_memory = path.starts_with("sqlite:") || path.contains("::memory:");
        if !is_memory {
            if let Some(parent) = Path::new(path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).map_err(PathwayError::Io)?;
                }
            }
        }
        let mut options = if is_memory {
            SqliteConnectOptions::from_str(path)
                .map_err(|e| PathwayError::Config(e.to_string()))?
        } else {
            SqliteConnectOptions::new().filename(path).create_if_missing(true)
        };
        // `foreign_keys`/`busy_timeout` are per-connection SQLite settings,
        // not persisted in the database file -- a one-off `PRAGMA` query
        // issued against the pool only touches whichever single connection
        // happened to service that query, leaving every other pooled
        // connection with `foreign_keys=OFF` and the default busy timeout.
        // These builder methods apply to every connection sqlx opens, which
        // is the correct place for them. `journal_mode`/`synchronous` are
        // harmless to set the same way (WAL is a no-op for `:memory:`, which
        // SQLite always keeps in an in-memory journal regardless).
        options = options
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_millis(5000));

        // A pool with >1 connection to `sqlite::memory:` hands out a
        // *distinct, empty* database per connection (confirmed by
        // `in_memory_database_is_isolated` in tests/store.rs, which exists
        // to document exactly this) -- capping to a single connection is
        // required for correctness, not just an optimization, whenever the
        // backing store is in-memory.
        //
        // For the real (file-backed) `pathway.db` too: a single connection
        // is what makes the explicit `BEGIN`/`COMMIT` wrapping in
        // `belief::synthesis::route_observation` and
        // `consolidate::consolidate_session` actually atomic -- sequential
        // `.execute(pool)` calls against a >1-connection pool aren't
        // guaranteed to land on the same physical connection, so a manually
        // issued `BEGIN` on connection A wouldn't cover a later statement
        // that happened to run on connection B. `pathway.db` is a small
        // side-database (never the chat hot path; writers are already
        // serialized by the per-session learn lock and the global
        // structured_chat semaphore), so this costs nothing in practice.
        let pool_options = SqlitePoolOptions::new().min_connections(1).max_connections(1);
        let pool = pool_options.connect_with(options).await?;
        Self::init(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn open_in_memory() -> Result<Self> {
        Self::open("sqlite::memory:").await
    }

    async fn init(pool: &SqlitePool) -> Result<()> {
        sqlx::migrate!("./migrations").run(pool).await.map_err(|e| {
            PathwayError::Migrate(e.to_string())
        })?;
        Ok(())
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Raw `BEGIN`/`COMMIT`/`ROLLBACK` over the pool. Correct *because* the
    /// pool is capped to a single connection (see `open`): sqlx has nowhere
    /// else to route a query, so sequential calls through these three and
    /// whatever runs between them are guaranteed to hit the same physical
    /// SQLite connection, making this atomic without needing every `Db`
    /// method to accept a shared `Transaction`/executor parameter. Callers
    /// MUST use `run_in_transaction` (below) rather than calling these
    /// directly -- a `?`-early-return between `begin_txn` and a commit/
    /// rollback would leave the single connection wedged inside an open
    /// transaction forever, blocking every subsequent query against this
    /// `Db`.
    async fn begin_txn(&self) -> Result<()> {
        sqlx::query("BEGIN").execute(self.pool()).await?;
        Ok(())
    }
    async fn commit_txn(&self) -> Result<()> {
        sqlx::query("COMMIT").execute(self.pool()).await?;
        Ok(())
    }
    async fn rollback_txn(&self) {
        // Best-effort: if the connection is already broken, there's nothing
        // more to roll back to -- but never let a rollback failure mask the
        // original error that triggered it.
        let _ = sqlx::query("ROLLBACK").execute(self.pool()).await;
    }

    /// Run `f` inside a `BEGIN`/`COMMIT` transaction, rolling back on any
    /// `Err`. This is the only sanctioned way to use `begin_txn`/
    /// `commit_txn`/`rollback_txn` -- it guarantees the transaction is
    /// always closed one way or the other, even if `f` returns early via
    /// `?`, so the single connection (see `open`) never gets stuck inside
    /// an open transaction.
    pub async fn run_in_transaction<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        self.begin_txn().await?;
        match f().await {
            Ok(value) => {
                self.commit_txn().await?;
                Ok(value)
            }
            Err(e) => {
                self.rollback_txn().await;
                Err(e)
            }
        }
    }
}

/// Encode an `f32` slice as a BLOB (little-endian bytes), for the
/// `embedding BLOB` columns.
pub fn encode_embedding(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Decode a BLOB back into `Vec<f32>` (little-endian, matching
/// `encode_embedding`). A byte count not divisible by 4 means the BLOB is
/// corrupt or was never written by `encode_embedding` -- this previously
/// claimed to "fall back to empty" in that case, which wasn't actually true:
/// `chunks_exact(4)` silently drops only the incomplete trailing bytes and
/// still returns however many complete f32s it found, which is a length
/// mismatch against whatever the caller expected, not an empty vector.
/// `cosine`'s own length guard is the real backstop against that (a
/// mismatched-length embedding compares as unrelated rather than a
/// misleading partial match) -- this just logs so a genuinely corrupt row is
/// visible instead of silently returning a truncated vector.
pub fn decode_embedding(bytes: &[u8]) -> Vec<f32> {
    if !bytes.is_empty() && !bytes.len().is_multiple_of(4) {
        tracing::warn!(
            "decode_embedding: {} bytes is not a multiple of 4 -- BLOB is corrupt or not an f32 vector; \
             decoding the {} complete f32s found and dropping the trailing {} byte(s)",
            bytes.len(),
            bytes.len() / 4,
            bytes.len() % 4,
        );
    }
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}
