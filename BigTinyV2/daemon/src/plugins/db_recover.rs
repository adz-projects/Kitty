//! Best-effort recovery for a corrupt plugin SQLite database (the pathway and
//! memorabilia side-DBs). The shape is the same for both engines:
//!
//! 1. Take the engine instance offline so its pool releases the file.
//! 2. `PRAGMA integrity_check`; if `ok`, checkpoint the WAL and reopen — nothing
//!    to rebuild.
//! 3. If corrupt: back up the file, create a fresh migrated DB, **salvage** as
//!    many rows as still read cleanly (a corrupt table is skipped, not fatal),
//!    swap the rebuilt file in, and reopen.
//!
//! Salvage copies plain base tables only. FTS5/vec0 virtual tables and their
//! shadow tables are skipped: FTS content is rebuilt by the base table's insert
//! triggers, and a vec0 index (memorabilia's `chunks_vec`) is recreated empty by
//! the engine's own open path — so after a rebuild, salvaged chunks keep their
//! rows but need re-embedding before they are vector-searchable again. In
//! practice only the pathway DB (which has no vec0 table) has been seen to
//! corrupt; this keeps the routine engine-agnostic all the same.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

/// Outcome of a recovery attempt, serialized straight to the recover endpoints.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct RecoverReport {
    /// Whether the database is `PRAGMA integrity_check` clean at the end.
    pub integrity_ok: bool,
    /// Whether a rebuild happened (false = it was already clean).
    pub rebuilt: bool,
    /// Rows salvaged per table (only present when `rebuilt`).
    pub salvaged: BTreeMap<String, i64>,
    /// Path of the pre-rebuild backup, when one was taken.
    pub backup: Option<String>,
    /// Set when the file checks out but the engine still fails to open (e.g. a
    /// migration mismatch) — so a sound file is never reported as "healthy"
    /// while memory is actually down.
    pub open_error: Option<String>,
}

/// Append a raw suffix to a path's filename (e.g. `pathway.db` + `-wal`).
pub fn append(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Open a throwaway single-connection pool on an *existing* file, without
/// running migrations (so a corrupt file can still be integrity-checked).
/// `None` when the file cannot be opened at all.
pub async fn open_existing(path: &Path) -> Option<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .busy_timeout(Duration::from_millis(5000));
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .ok()
}

/// Run `PRAGMA integrity_check`, returning true iff the first row is `ok`.
pub async fn integrity_ok(pool: &SqlitePool) -> bool {
    match sqlx::query_scalar::<_, String>("PRAGMA integrity_check")
        .fetch_one(pool)
        .await
    {
        Ok(s) => s.eq_ignore_ascii_case("ok"),
        Err(_) => false,
    }
}

/// Flush the WAL back into the main file (best-effort).
pub async fn checkpoint_truncate(pool: &SqlitePool) {
    let _ = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(pool)
        .await;
}

/// A vec0/FTS5 shadow (or virtual) table we must not copy directly.
fn is_shadow(name: &str) -> bool {
    name.contains("_fts") || name.contains("_vec")
}

/// Double-quote an identifier for safe interpolation into SQL.
fn quote_ident(id: &str) -> String {
    format!("\"{}\"", id.replace('"', "\"\""))
}

/// Column names of `schema.table` (`main` or the attached `old`).
async fn columns(pool: &SqlitePool, schema: &str, table: &str) -> Vec<String> {
    let sql = format!("PRAGMA {schema}.table_info({})", quote_ident(table));
    match sqlx::query(&sql).fetch_all(pool).await {
        Ok(rows) => rows.into_iter().map(|r| r.get::<String, _>("name")).collect(),
        Err(_) => Vec::new(),
    }
}

/// Copy salvageable rows from the corrupt DB at `source_path` into the fresh,
/// already-migrated `dest` pool. Per-table best-effort: a corrupt or missing
/// table is skipped. Returns rows copied per table.
pub async fn salvage_into(dest: &SqlitePool, source_path: &Path) -> BTreeMap<String, i64> {
    let mut salvaged = BTreeMap::new();

    let src = source_path.to_string_lossy().to_string();
    if sqlx::query("ATTACH DATABASE ? AS old")
        .bind(&src)
        .execute(dest)
        .await
        .is_err()
    {
        return salvaged; // can't read the corrupt file at all
    }
    // Copy across foreign keys (child rows before parents) without ordering.
    let _ = sqlx::query("PRAGMA foreign_keys=OFF").execute(dest).await;

    // Destination base tables: plain `CREATE TABLE`, not sqlite internals,
    // migrations, or FTS/vec shadow tables.
    let tables: Vec<String> = match sqlx::query(
        "SELECT name FROM sqlite_master WHERE type='table' \
         AND sql LIKE 'CREATE TABLE%' AND name NOT LIKE 'sqlite_%' AND name <> '_sqlx_migrations'",
    )
    .fetch_all(dest)
    .await
    {
        Ok(rows) => rows.into_iter().map(|r| r.get::<String, _>("name")).collect(),
        Err(_) => Vec::new(),
    };

    for t in tables {
        if is_shadow(&t) {
            continue;
        }
        // Does the corrupt source even have this table?
        let src_has: Option<String> =
            sqlx::query_scalar("SELECT name FROM old.sqlite_master WHERE type='table' AND name = ?")
                .bind(&t)
                .fetch_optional(dest)
                .await
                .ok()
                .flatten();
        if src_has.is_none() {
            continue;
        }
        // Copy only columns present in both (tolerates schema drift).
        let dcols = columns(dest, "main", &t).await;
        let scols = columns(dest, "old", &t).await;
        let shared: Vec<String> = dcols.into_iter().filter(|c| scols.contains(c)).collect();
        if shared.is_empty() {
            continue;
        }
        let cols = shared
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        let qt = quote_ident(&t);
        let sql = format!("INSERT OR IGNORE INTO main.{qt} ({cols}) SELECT {cols} FROM old.{qt}");
        match sqlx::query(&sql).execute(dest).await {
            Ok(r) => {
                salvaged.insert(t, r.rows_affected() as i64);
            }
            Err(e) => tracing::warn!("recover: skipped corrupt table {t}: {e}"),
        }
    }

    let _ = sqlx::query("PRAGMA foreign_keys=ON").execute(dest).await;
    let _ = sqlx::query("DETACH DATABASE old").execute(dest).await;
    salvaged
}

/// Copy `src` to `dst` if `src` exists; a missing source is not an error.
async fn copy_if_exists(src: &Path, dst: &Path) {
    if tokio::fs::try_exists(src).await.unwrap_or(false) {
        if let Err(e) = tokio::fs::copy(src, dst).await {
            tracing::warn!("recover: could not copy {src:?} -> {dst:?}: {e}");
        }
    }
}

/// Back up the corrupt DB (and its `-wal`/`-shm` siblings) next to it, returning
/// the backup's path.
pub async fn backup_corrupt(db_path: &Path) -> PathBuf {
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let backup = append(db_path, &format!(".corrupt-{ts}.bak"));
    copy_if_exists(db_path, &backup).await;
    copy_if_exists(&append(db_path, "-wal"), &append(&backup, "-wal")).await;
    copy_if_exists(&append(db_path, "-shm"), &append(&backup, "-shm")).await;
    backup
}

/// Remove the old DB (+ `-wal`/`-shm`) and move `rebuilt` into `db_path`.
///
/// Retried: after an engine instance is closed, the aborted background task and
/// sqlx's own pool teardown can hold the OS file handle for a short window, and
/// Windows refuses to remove/rename a file with an open handle. A few spaced
/// retries ride out that window.
pub async fn swap_in(rebuilt: &Path, db_path: &Path) -> std::io::Result<()> {
    let remove = |p: PathBuf| async move {
        for attempt in 0..20u32 {
            match tokio::fs::remove_file(&p).await {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) if attempt == 19 => return Err(e),
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        Ok(())
    };
    remove(db_path.to_path_buf()).await?;
    let _ = remove(append(db_path, "-wal")).await;
    let _ = remove(append(db_path, "-shm")).await;

    for attempt in 0..20u32 {
        match tokio::fs::rename(rebuilt, db_path).await {
            Ok(()) => return Ok(()),
            Err(e) if attempt == 19 => return Err(e),
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    Ok(())
}

/// Remove a stale rebuilt file (and siblings) from a previous aborted attempt.
pub async fn clear_rebuilt(rebuilt: &Path) {
    let _ = tokio::fs::remove_file(rebuilt).await;
    let _ = tokio::fs::remove_file(append(rebuilt, "-wal")).await;
    let _ = tokio::fs::remove_file(append(rebuilt, "-shm")).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn open_create(path: &Path) -> SqlitePool {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn salvage_copies_shared_columns_and_skips_shadow_tables() {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("source.db");
        let dst_path = dir.path().join("dest.db");

        // Source: a base table with rows, a since-dropped column, and a vec-like
        // shadow table that salvage must skip.
        {
            let src = open_create(&src_path).await;
            sqlx::query("CREATE TABLE beliefs (id TEXT PRIMARY KEY, text TEXT, gone TEXT)")
                .execute(&src)
                .await
                .unwrap();
            for i in 0..3 {
                sqlx::query("INSERT INTO beliefs (id, text, gone) VALUES (?, ?, ?)")
                    .bind(format!("b{i}"))
                    .bind(format!("belief {i}"))
                    .bind("stale")
                    .execute(&src)
                    .await
                    .unwrap();
            }
            sqlx::query("CREATE TABLE beliefs_vec (id TEXT, v BLOB)")
                .execute(&src)
                .await
                .unwrap();
            sqlx::query("INSERT INTO beliefs_vec (id, v) VALUES ('b0', x'00')")
                .execute(&src)
                .await
                .unwrap();
            src.close().await;
        }

        // Fresh destination with the current schema (no `gone` column, and the
        // vec shadow present but never a copy target).
        let dst = open_create(&dst_path).await;
        sqlx::query("CREATE TABLE beliefs (id TEXT PRIMARY KEY, text TEXT)")
            .execute(&dst)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE beliefs_vec (id TEXT, v BLOB)")
            .execute(&dst)
            .await
            .unwrap();

        let salvaged = salvage_into(&dst, &src_path).await;

        assert_eq!(salvaged.get("beliefs"), Some(&3));
        assert!(!salvaged.contains_key("beliefs_vec"), "shadow table skipped");
        let copied: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM beliefs")
            .fetch_one(&dst)
            .await
            .unwrap();
        assert_eq!(copied, 3);
        assert!(integrity_ok(&dst).await);
        dst.close().await;
    }

    #[tokio::test]
    async fn integrity_ok_true_on_a_healthy_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("healthy.db");
        let pool = open_create(&path).await;
        sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        assert!(integrity_ok(&pool).await);
        pool.close().await;
    }
}
