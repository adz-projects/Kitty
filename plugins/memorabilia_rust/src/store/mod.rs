//! Persistent storage for the factual-memory engine. Owns `memorabilia.db`
//! with its own `sqlx::migrate!` chain. PRAGMAs match the behavioral-memory
//! reference: WAL, synchronous=NORMAL, foreign_keys, busy_timeout.

use std::path::Path;
use std::str::FromStr;
use std::sync::Once;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;

use crate::error::{Error, Result};

pub mod audit;
pub mod chunks;
pub mod documents;
pub mod edges;
pub mod outbox;
pub mod propositions;
pub mod registry;
pub mod tombstones;
pub mod vectors;

/// Register the statically linked sqlite-vec `vec0` module (plan §3.3) on
/// every SQLite connection this process opens. The `sqlite-vec` crate ships
/// the extension compiled as a static `sqlite_vec0` library whose C code
/// references the core `sqlite3_*` symbols the process already links from
/// libsqlite3-sys's bundled build — so no second SQLite core exists, and an
/// `auto_extension` callback registered before the first `sqlite3_open`
/// applies the module to sqlx's connections. `Once` keeps repeat `Db::open`
/// calls idempotent (registering the module twice would double-initialize
/// it per connection).
fn register_sqlite_vec() {
    static REGISTERED: Once = Once::new();
    REGISTERED.call_once(|| unsafe {
        libsqlite3_sys::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
}

pub struct Db {
    pool: SqlitePool,
    /// Serializes `run_in_transaction` on the single shared connection, from
    /// `BEGIN` to `COMMIT`/`ROLLBACK`, so two tasks' transactions can never
    /// interleave, and so the abandoned-transaction rollback (see
    /// [`TxnGuard`]) always finishes before the next transaction begins.
    txn_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
}

/// Rolls back a transaction whose future was dropped before it finished.
///
/// `run_in_transaction` issues raw `BEGIN`/`COMMIT` on a one-connection pool.
/// If the future is dropped in between (a caller's `tokio::time::timeout`, a
/// turn cancelling its background work, a task `abort()`), nothing ever sends
/// `COMMIT` or `ROLLBACK`. The shared connection then stays inside that
/// transaction for the rest of the process, and every later write silently
/// joins it: visible to this process, never committed, lost on restart.
/// `Drop` can't `.await`, so it spawns the `ROLLBACK`, handing it the owned
/// `txn_lock` guard so the next transaction waits until the rollback is done.
struct TxnGuard {
    pool: SqlitePool,
    lock: Option<tokio::sync::OwnedMutexGuard<()>>,
    armed: bool,
}

impl Drop for TxnGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        tracing::warn!("memorabilia: transaction abandoned before commit; rolling it back");
        let lock = self.lock.take();
        let pool = self.pool.clone();
        // No runtime (process teardown): the connection closes with the
        // process, and `begin_txn`'s stale-transaction recovery covers the rest.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = sqlx::query("ROLLBACK").execute(&pool).await;
                drop(lock);
            });
        }
    }
}

impl Db {
    /// Open (creating if needed) the DB at `path`, apply PRAGMAs +
    /// migrations. `path` may be a file path or a `sqlite:` URL (including
    /// `sqlite::memory:` for tests).
    pub async fn open(path: &str) -> Result<Self> {
        register_sqlite_vec();
        let is_memory = path.starts_with("sqlite:") || path.contains("::memory:");
        if !is_memory {
            if let Some(parent) = Path::new(path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).map_err(Error::Io)?;
                }
            }
        }
        let mut options = if is_memory {
            SqliteConnectOptions::from_str(path).map_err(|e| Error::Config(e.to_string()))?
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
        // harmless to set the same way (WAL is a no-op for `:memory:`).
        options = options
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_millis(5000));

        // A pool with >1 connection to `sqlite::memory:` hands out a
        // *distinct, empty* database per connection -- capping to a single
        // connection is required for correctness whenever the backing store
        // is in-memory. For the file-backed DB, a single connection is what
        // makes the explicit `BEGIN`/`COMMIT` wrapping in
        // `run_in_transaction` actually atomic: sequential `.execute(pool)`
        // calls against a >1-connection pool are not guaranteed to land on
        // the same physical connection, so a manually issued `BEGIN` on
        // connection A would not cover a later statement run on connection B.
        // `memorabilia.db` is a small side-database (writers are serialized
        // by the extraction semaphore and the maintenance lock), so this
        // costs nothing in practice.
        let pool_options = SqlitePoolOptions::new().min_connections(1).max_connections(1);
        let pool = pool_options.connect_with(options).await?;
        Self::init(&pool).await?;
        Ok(Self {
            pool,
            txn_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Open an in-memory database for tests: clean, fast state per test
    /// (claude.md, testing discipline).
    pub async fn open_in_memory() -> Result<Self> {
        Self::open("sqlite::memory:").await
    }

    async fn init(pool: &SqlitePool) -> Result<()> {
        let migrator = sqlx::migrate!("./migrations");
        reconcile_line_ending_checksums(pool, &migrator).await;
        migrator
            .run(pool)
            .await
            .map_err(|e| Error::Migrate(e.to_string()))?;
        Ok(())
    }

    /// Create the `chunks_vec` vec0 virtual table sized to `dim` (plan
    /// §3.3). Created in code rather than a migration because the
    /// `float[<dim>]` column type encodes the configured `embedding_dim` —
    /// a value the static migration files cannot know. Idempotent
    /// (`IF NOT EXISTS`); call once after `open`/`open_in_memory`, before
    /// any [`super::store::vectors::SqliteVectorIndex`] use.
    pub async fn init_vectors(&self, dim: usize) -> Result<()> {
        let sql = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS chunks_vec USING vec0(\
             chunk_id TEXT PRIMARY KEY, embedding float[{dim}])"
        );
        sqlx::query(&sql).execute(self.pool()).await?;
        Ok(())
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Raw `BEGIN`/`COMMIT`/`ROLLBACK` over the pool. Correct *because* the
    /// pool is capped to a single connection (see `open`): sequential calls
    /// are guaranteed to hit the same physical SQLite connection. Callers
    /// MUST use `run_in_transaction` (below) rather than calling `begin_txn`
    /// directly -- a `?`-early-return between begin and commit/rollback would
    /// leave the single connection wedged inside an open transaction forever.
    async fn begin_txn(&self) -> Result<()> {
        match sqlx::query("BEGIN").execute(self.pool()).await {
            Ok(_) => Ok(()),
            // A transaction from an earlier, abandoned caller is still open on
            // the shared connection. `txn_lock` guarantees it isn't a live
            // one, so roll it back and start ours rather than failing (and
            // leaving it open) forever.
            Err(e) if e.to_string().contains("within a transaction") => {
                tracing::warn!(
                    "memorabilia: stale open transaction on the shared connection; rolling it back"
                );
                let _ = sqlx::query("ROLLBACK").execute(self.pool()).await;
                sqlx::query("BEGIN").execute(self.pool()).await?;
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }
    async fn commit_txn(&self) -> Result<()> {
        sqlx::query("COMMIT").execute(self.pool()).await?;
        Ok(())
    }
    async fn rollback_txn(&self) {
        // Best-effort: if the connection is already broken there is nothing
        // to roll back to -- but never let a rollback failure mask the
        // original error (soft-fail discipline, claude.md core principle 9).
        let _ = sqlx::query("ROLLBACK").execute(self.pool()).await;
    }

    /// Read a scalar `app_settings` value (001: `key`/`value`).
    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT value FROM app_settings WHERE key = ?")
            .bind(key)
            .fetch_optional(self.pool())
            .await?)
    }

    /// Upsert a scalar `app_settings` value (e.g. the maintenance heavy-pass
    /// cadence anchor `last_maintenance_at`, plan §11).
    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO app_settings (key, value) VALUES (?, ?)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// The Stage 4 extraction watermark: the highest chunk rowid this
    /// worker has ever taken to `extraction_status = 'done'` (plan §3.6,
    /// mirroring the reference's forward-only `last_learned_rowid`
    /// discipline). Bookkeeping by design — `extraction_status` is the
    /// primary double-extraction guard; the watermark is the durable
    /// forward-only bound that keeps a regressed row from ever being
    /// re-extracted.
    pub async fn read_extraction_watermark(&self) -> i64 {
        match self.get_setting("extraction_watermark").await {
            Ok(Some(v)) => v.parse().unwrap_or(0),
            _ => 0,
        }
    }

    /// Advance the watermark, never backward (`MAX` guard).
    pub async fn advance_extraction_watermark(&self, rowid: i64) -> Result<()> {
        let current = self.read_extraction_watermark().await;
        if rowid > current {
            sqlx::query(
                "INSERT INTO app_settings (key, value) VALUES ('extraction_watermark', ?)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            )
            .bind(rowid.to_string())
            .execute(self.pool())
            .await?;
        }
        Ok(())
    }

    /// Set the per-session pause flag (Kitty's unified "pause memory"
    /// control). Upserts so a fresh session with no prior row still persists
    /// — the pause can be set before memorabilia has seen the session at all.
    pub async fn set_paused(&self, session_id: &str, paused: bool) -> Result<()> {
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        sqlx::query(
            "INSERT INTO session_pause (session_id, paused, updated_at) VALUES (?, ?, ?)
             ON CONFLICT (session_id) DO UPDATE SET
                 paused = excluded.paused, updated_at = excluded.updated_at",
        )
        .bind(session_id)
        .bind(paused)
        .bind(now)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Whether recall/learn are paused for `session_id` (absent row = not
    /// paused). DB-backed rather than an in-memory map so the flag survives a
    /// daemon restart.
    pub async fn is_paused(&self, session_id: &str) -> Result<bool> {
        let paused: Option<bool> =
            sqlx::query_scalar("SELECT paused FROM session_pause WHERE session_id = ?")
                .bind(session_id)
                .fetch_optional(self.pool())
                .await?;
        Ok(paused.unwrap_or(false))
    }

    /// Run `f` inside a `BEGIN`/`COMMIT` transaction, rolling back on any
    /// `Err`. The only sanctioned way to use the raw txn helpers: the
    /// transaction is always closed one way or the other — when `f` returns
    /// early via `?`, when `COMMIT` itself fails, and (via [`TxnGuard`]) when
    /// this future is dropped mid-flight by a timeout or task abort.
    pub async fn run_in_transaction<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let lock = self.txn_lock.clone().lock_owned().await;
        // Armed before `BEGIN`: a drop while `BEGIN` is in flight still rolls
        // back (harmlessly, if it never ran).
        let mut guard = TxnGuard {
            pool: self.pool.clone(),
            lock: Some(lock),
            armed: true,
        };
        if let Err(e) = self.begin_txn().await {
            guard.armed = false;
            return Err(e);
        }
        let result = match f().await {
            Ok(value) => match self.commit_txn().await {
                Ok(()) => Ok(value),
                // A failed COMMIT (e.g. SQLITE_BUSY) leaves the transaction
                // open; close it rather than stranding the connection.
                Err(e) => {
                    self.rollback_txn().await;
                    Err(e)
                }
            },
            Err(e) => {
                self.rollback_txn().await;
                Err(e)
            }
        };
        guard.armed = false;
        result
    }
}

/// Make sqlx's applied-migration check tolerant of line-ending drift.
///
/// sqlx records a SHA-384 of each migration file's *raw bytes* and refuses to
/// open a database whose recorded checksum differs from the one compiled in.
/// A migration checked out with CRLF on one build and LF on the next (the
/// repo's `.gitattributes` now forces LF) is byte-different but SQL-identical,
/// and that alone bricked the engine ("migration 3 was previously applied but
/// has been modified"). For each applied migration whose recorded checksum
/// matches the LF *or* CRLF form of the embedded SQL, rewrite it to the
/// embedded checksum. Any other mismatch is left alone, so a genuinely edited
/// migration still fails loudly. Best-effort: on a fresh DB (no
/// `_sqlx_migrations` yet) or any query error this is a no-op and the normal
/// migrator runs as before.
pub(crate) async fn reconcile_line_ending_checksums(
    pool: &SqlitePool,
    migrator: &sqlx::migrate::Migrator,
) {
    use sha2::{Digest, Sha384};

    let has_table: Option<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    if has_table.is_none() {
        return;
    }

    for m in migrator.iter() {
        let stored: Option<Vec<u8>> =
            match sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = ?")
                .bind(m.version)
                .fetch_optional(pool)
                .await
            {
                Ok(v) => v,
                Err(_) => continue,
            };
        let Some(stored) = stored else { continue };
        if stored.as_slice() == m.checksum.as_ref() {
            continue;
        }
        let lf = m.sql.replace("\r\n", "\n");
        let crlf = lf.replace('\n', "\r\n");
        let equivalent = [lf, crlf]
            .iter()
            .any(|variant| Sha384::digest(variant.as_bytes()).as_slice() == stored.as_slice());
        if !equivalent {
            continue;
        }
        if sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = ?")
            .bind(m.checksum.as_ref())
            .bind(m.version)
            .execute(pool)
            .await
            .is_ok()
        {
            tracing::info!(
                version = m.version,
                "memorabilia: migration checksum differed only by line endings; reconciled"
            );
        }
    }
}

/// Encode an `f32` slice as a BLOB (little-endian bytes), for `embedding`
/// BLOB columns (Phase 2+).
pub fn encode_embedding(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Decode a BLOB back into `Vec<f32>` (little-endian, matching
/// [`encode_embedding`]). A byte count not divisible by 4 means the BLOB is
/// corrupt; the trailing partial f32 is dropped and a warning is logged so a
/// genuinely corrupt row is visible instead of silently truncated.
pub fn decode_embedding(bytes: &[u8]) -> Vec<f32> {
    if !bytes.is_empty() && bytes.len() % 4 != 0 {
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
