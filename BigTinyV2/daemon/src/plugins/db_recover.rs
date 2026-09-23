//! Best-effort recovery for a corrupt plugin SQLite database (the pathway and
//! memorabilia side-DBs). The shape is the same for both engines:
//!
//! 1. **Check** ([`check_file`]): `PRAGMA integrity_check` on a *copy* of the
//!    file and its WAL — opening the original would checkpoint a damaged WAL
//!    into it. Safe at any time; the Settings "Check & repair" button uses it
//!    and never touches a live engine or the file itself.
//! 2. **Heal** ([`heal_before_open`]): on the first open of an app's engine in
//!    a daemon process — before anything else holds the file — a failing file
//!    is backed up, a fresh migrated DB is created, as many rows as still read
//!    cleanly are **salvaged** (a corrupt table is skipped, not fatal), and the
//!    rebuilt file is swapped in. A corrupt file found by the button is
//!    therefore repaired by restarting the backend.
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
    /// The file is corrupt and must be rebuilt, which only happens safely
    /// before any engine has it open — i.e. on the next daemon start (see
    /// [`heal_before_open`]). The client restarts the backend and re-checks.
    pub restart_required: bool,
}

/// Before an engine opens `db_path` for the first time in this process: if the
/// file exists and fails `PRAGMA integrity_check`, back it up and rebuild it
/// from whatever still reads. Returns the report when a rebuild was attempted,
/// `None` when the file was absent or healthy.
///
/// This is the only place a live database file is replaced. At first open
/// nothing else holds a connection to it, which is what makes the swap safe;
/// swapping under a running engine (which the in-process MCP servers keep
/// alive via their own `Arc`) is exactly what must never happen.
/// `create_fresh` creates a new, fully migrated database at the given path and
/// returns its pool (the engine crate's own `Db::open`).
pub async fn heal_before_open<F, Fut>(
    label: &str,
    db_path: &Path,
    create_fresh: F,
) -> Option<RecoverReport>
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<SqlitePool, String>>,
{
    if check_file(db_path).await != Some(false) {
        return None; // absent or healthy
    }
    tracing::warn!("{label}: {db_path:?} failed its integrity check; rebuilding from what still reads");
    Some(rebuild(label, db_path, create_fresh).await)
}

/// Back up `db_path`, create a fresh migrated database beside it, salvage every
/// table that still reads, and swap it into place. The caller guarantees no
/// connection holds `db_path` (see [`heal_before_open`]).
async fn rebuild<F, Fut>(label: &str, db_path: &Path, create_fresh: F) -> RecoverReport
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<SqlitePool, String>>,
{
    let mut report = RecoverReport {
        rebuilt: true,
        ..Default::default()
    };
    report.backup = Some(backup_corrupt(db_path).await.to_string_lossy().into_owned());
    let rebuilt = append(db_path, ".rebuilt");
    clear_rebuilt(&rebuilt).await;

    match create_fresh(rebuilt.clone()).await {
        Ok(pool) => {
            report.salvaged = salvage_into(&pool, db_path).await;
            report.integrity_ok = integrity_ok(&pool).await;
            pool.close().await;
        }
        Err(e) => {
            tracing::warn!("{label}: could not create a fresh database to rebuild into: {e}");
            clear_rebuilt(&rebuilt).await;
            return report;
        }
    }
    match swap_in(&rebuilt, db_path).await {
        Ok(()) => tracing::info!(
            backup = ?report.backup,
            salvaged = ?report.salvaged,
            "{label}: rebuilt corrupt database"
        ),
        Err(e) => {
            tracing::warn!("{label}: could not swap the rebuilt database into {db_path:?}: {e}");
            report.integrity_ok = false;
        }
    }
    report
}

/// Integrity-check `db_path` without touching it: the check runs on a copy of
/// the file and its WAL. `None` when the file doesn't exist.
///
/// Never on the original. Closing the last connection to a WAL database
/// checkpoints the WAL into the main file, so merely opening a damaged file to
/// look at it would fold the damage into a main file that may itself be sound
/// (the field case: a healthy main file with a WAL that doesn't match it),
/// destroying the state a rebuild would salvage from. A copy taken while an
/// engine is writing can occasionally mismatch; the cost of that is a
/// spurious restart, after which the check runs with nothing writing.
pub async fn check_file(db_path: &Path) -> Option<bool> {
    if !tokio::fs::try_exists(db_path).await.unwrap_or(false) {
        return None;
    }
    let snap = append(db_path, ".check");
    clear_rebuilt(&snap).await;
    if let Err(e) = tokio::fs::copy(db_path, &snap).await {
        // Can't tell (e.g. disk full) — which must never read as "corrupt"
        // and trigger a rebuild.
        tracing::warn!("recover: could not snapshot {db_path:?} to check it: {e}");
        clear_rebuilt(&snap).await;
        return None;
    }
    copy_if_exists(&append(db_path, "-wal"), &append(&snap, "-wal")).await;
    let ok = match open_existing(&snap).await {
        Some(pool) => {
            let ok = integrity_ok(&pool).await;
            pool.close().await;
            ok
        }
        None => false,
    };
    clear_rebuilt(&snap).await;
    Some(ok)
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

/// Copy one table's rows from the attached `schema` into `main`, over the
/// columns both sides have (tolerates schema drift). `Ok(None)` when the source
/// lacks the table or shares no columns with it. A single `INSERT … SELECT` is
/// atomic, so an `Err` leaves nothing half-copied to retry over.
async fn copy_table(dest: &SqlitePool, schema: &str, table: &str) -> Result<Option<i64>, String> {
    let has: Option<String> = sqlx::query_scalar(&format!(
        "SELECT name FROM {schema}.sqlite_master WHERE type='table' AND name = ?"
    ))
    .bind(table)
    .fetch_optional(dest)
    .await
    .map_err(|e| e.to_string())?;
    if has.is_none() {
        return Ok(None);
    }
    let dcols = columns(dest, "main", table).await;
    let scols = columns(dest, schema, table).await;
    let shared: Vec<String> = dcols.into_iter().filter(|c| scols.contains(c)).collect();
    if shared.is_empty() {
        return Ok(None);
    }
    let cols = shared
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let qt = quote_ident(table);
    let sql = format!("INSERT OR IGNORE INTO main.{qt} ({cols}) SELECT {cols} FROM {schema}.{qt}");
    sqlx::query(&sql)
        .execute(dest)
        .await
        .map(|r| Some(r.rows_affected() as i64))
        .map_err(|e| e.to_string())
}

/// Copy salvageable rows from the corrupt DB at `source_path` into the fresh,
/// already-migrated `dest` pool. Returns rows copied per table.
///
/// Two passes. First from the file as SQLite sees it — main file plus the
/// WAL's committed frames — which is the newest state. Any table that can't be
/// read that way is then retried from the **main file alone** (a WAL-free copy
/// of it: the last checkpoint). That is what recovers the case seen in the
/// field: a WAL whose commits describe a smaller database than the main file,
/// so every table with pages past that size (e.g. `beliefs`, whose embedding
/// BLOBs live in overflow pages) reads as "malformed", while the main file on
/// its own is perfectly sound. Tables that fail both ways are skipped.
pub async fn salvage_into(dest: &SqlitePool, source_path: &Path) -> BTreeMap<String, i64> {
    let mut salvaged = BTreeMap::new();

    // Snapshot the main file WITHOUT its WAL first, before anything attaches
    // the original: when the last connection to a WAL database closes (our
    // DETACH below), SQLite checkpoints the WAL into the main file, which
    // would overwrite the last-checkpoint state with the very pages that are
    // corrupt. The WAL is only replayed onto a file that has a `-wal` beside
    // it, so this copy reads as the main file alone.
    let base = append(source_path, ".salvage-base");
    clear_rebuilt(&base).await;
    let have_base = tokio::fs::copy(source_path, &base).await.is_ok();

    let src = source_path.to_string_lossy().to_string();
    if sqlx::query("ATTACH DATABASE ? AS old")
        .bind(&src)
        .execute(dest)
        .await
        .is_err()
    {
        clear_rebuilt(&base).await;
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

    let mut failed: Vec<String> = Vec::new();
    for t in tables.into_iter().filter(|t| !is_shadow(t)) {
        match copy_table(dest, "old", &t).await {
            Ok(Some(n)) => {
                salvaged.insert(t, n);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("recover: {t} unreadable in the current state ({e}); will retry from the last checkpoint");
                failed.push(t);
            }
        }
    }
    let _ = sqlx::query("DETACH DATABASE old").execute(dest).await;

    if !failed.is_empty() {
        let attached = have_base
            && sqlx::query("ATTACH DATABASE ? AS base")
                .bind(base.to_string_lossy().to_string())
                .execute(dest)
                .await
                .is_ok();
        if attached {
            for t in failed {
                match copy_table(dest, "base", &t).await {
                    Ok(Some(n)) => {
                        tracing::info!("recover: salvaged {t} ({n} rows) from the last checkpoint");
                        salvaged.insert(t, n);
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("recover: skipped corrupt table {t}: {e}"),
                }
            }
            let _ = sqlx::query("DETACH DATABASE base").execute(dest).await;
        }
    }
    clear_rebuilt(&base).await;

    let _ = sqlx::query("PRAGMA foreign_keys=ON").execute(dest).await;
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

    /// The fixture "migration": two tables, so one can be corrupted and the
    /// other still salvaged.
    async fn create_schema(pool: &SqlitePool) {
        for t in ["alpha", "beta"] {
            sqlx::query(&format!("CREATE TABLE {t} (id INTEGER PRIMARY KEY, v TEXT)"))
                .execute(pool)
                .await
                .unwrap();
        }
    }

    /// A rollback-journal DB (no WAL, so every byte is in the main file) with a
    /// few pages per table, then one of `alpha`'s pages overwritten with junk.
    async fn corrupt_db(path: &Path) {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Delete);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        create_schema(&pool).await;
        let filler = "x".repeat(200);
        for t in ["alpha", "beta"] {
            for i in 0..60 {
                sqlx::query(&format!("INSERT INTO {t} (id, v) VALUES (?, ?)"))
                    .bind(i)
                    .bind(&filler)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        }
        let alpha_root: i64 =
            sqlx::query_scalar("SELECT rootpage FROM sqlite_master WHERE name = 'alpha'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&pool)
            .await
            .unwrap();
        pool.close().await;
        let mut bytes = std::fs::read(path).unwrap();
        let start = ((alpha_root - 1) * page_size) as usize;
        bytes[start..start + page_size as usize].fill(0xA5);
        std::fs::write(path, bytes).unwrap();
    }

    #[tokio::test]
    async fn check_file_reports_corruption_without_touching_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.db");
        corrupt_db(&path).await;
        let before = std::fs::read(&path).unwrap();

        assert_eq!(check_file(&path).await, Some(false));
        assert_eq!(std::fs::read(&path).unwrap(), before, "a check never modifies the file");
        assert_eq!(check_file(&dir.path().join("absent.db")).await, None);
    }

    #[tokio::test]
    async fn heal_before_open_rebuilds_a_corrupt_file_and_salvages_what_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.db");
        corrupt_db(&path).await;

        let report = heal_before_open("test", &path, |p| async move {
            let opts = SqliteConnectOptions::new().filename(&p).create_if_missing(true);
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(opts)
                .await
                .map_err(|e| e.to_string())?;
            create_schema(&pool).await;
            Ok(pool)
        })
        .await
        .expect("a corrupt file is rebuilt");

        assert!(report.rebuilt);
        assert!(report.integrity_ok);
        assert_eq!(report.salvaged.get("beta"), Some(&60), "the intact table is salvaged");
        let backup = report.backup.expect("a backup is taken");
        assert!(Path::new(&backup).exists(), "backup kept at {backup}");
        assert_eq!(check_file(&path).await, Some(true), "the swapped-in file is clean");

        // Healthy now, so a second heal is a no-op.
        let again = heal_before_open("test", &path, |_| async { Err("must not rebuild".to_string()) })
            .await;
        assert!(again.is_none());
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
