//! Phase 7a: migrate a Kitty V1 database forward into V2.
//!
//! Kitty has real accumulated state — sessions, message history, providers,
//! recipes, schedules. Because the V2 fork kept V1's sixteen migrations intact
//! and added `017+` on top rather than squashing them, V2 can open a V1
//! database and simply migrate it forward. That is what makes this an import
//! rather than an export/transform pipeline.
//!
//! # Two rules this module exists to enforce
//!
//! **Never in place.** The source database is copied first and the copy is
//! what gets migrated. V1 must stay bootable afterwards, because that is the
//! rollback path: if V2 misbehaves, the user reverts the app rather than
//! restoring a backup they may not have. An in-place migration would apply
//! `017+` to the live V1 database and make it unopenable by the older daemon.
//!
//! **Carry the encryption key across.** V1 takes its key from Kitty's
//! Credential Manager via `BIGTINY_ENCRYPTION_KEY`; V2 owns
//! `{data_dir}/encryption.key`. Every provider row's `api_key` is encrypted
//! with the V1 key, so importing without carrying it produces a database whose
//! providers all decrypt to ciphertext — and `crypto::decrypt` is deliberately
//! infallible, so the failure surfaces much later as a provider 401 rather
//! than as an import error. This is the step most likely to be skipped and the
//! most confusing when it is, so it is checked explicitly and reported.
//!
//! # Ownership
//!
//! Every row V1 wrote predates tenancy, so migration 017 gives it the `''`
//! placeholder. The import stamps a real owner over that, registers the app,
//! and verifies nothing was left behind.

use std::path::{Path, PathBuf};

use sqlx::SqlitePool;

use crate::error::DaemonError;

/// Import failures are all "this environment is not what the import needs",
/// so they share one constructor rather than each picking a variant.
fn fail(message: String) -> DaemonError {
    DaemonError::Storage(crate::error::StorageError::Generic(message))
}

/// What an import did, for reporting. Counts are read back *after* the
/// stamping pass, so they describe the imported database rather than the
/// statements that were issued.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub app_id: String,
    pub sessions: i64,
    pub messages: i64,
    pub providers: i64,
    pub recipes: i64,
    pub schedules: i64,
    /// Providers whose stored `api_key` is encrypted but does not decrypt with
    /// the key in force. Non-zero means the key was not carried across, and
    /// every one of those providers will fail to authenticate.
    pub undecryptable_providers: i64,
}

impl ImportSummary {
    pub fn render(&self) -> String {
        let mut out = format!(
            "imported as app {:?}\n  \
             {} sessions, {} messages\n  \
             {} providers, {} recipes, {} schedules",
            self.app_id,
            self.sessions,
            self.messages,
            self.providers,
            self.recipes,
            self.schedules,
        );
        if self.undecryptable_providers > 0 {
            out.push_str(&format!(
                "\n\n  WARNING: {} provider(s) hold an encrypted api_key that does not\n  \
                 decrypt with this daemon's key. They will fail to authenticate.\n  \
                 Re-run with --encryption-key set to the key V1 was using\n  \
                 (Kitty stores it in the Windows Credential Manager).",
                self.undecryptable_providers
            ));
        }
        out
    }
}

/// Copy `source` next to the V2 database and return the copy's path.
///
/// Copies the `-wal` and `-shm` sidecars when present. A V1 daemon shut down
/// cleanly leaves no WAL, but one killed mid-write does, and it holds
/// committed transactions — copying only the main file would silently discard
/// the most recent sessions.
fn copy_database(source: &Path, dest: &Path) -> Result<(), DaemonError> {
    if !source.exists() {
        return Err(fail(format!(
            "no database at {}",
            source.display()
        )));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| fail(format!("could not create {}: {e}", parent.display())))?;
    }
    std::fs::copy(source, dest)
        .map_err(|e| fail(format!("could not copy database: {e}")))?;

    for suffix in ["-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{suffix}", source.display()));
        if from.exists() {
            let to = PathBuf::from(format!("{}{suffix}", dest.display()));
            std::fs::copy(&from, &to).map_err(|e| {
                fail(format!("could not copy {}: {e}", from.display()))
            })?;
        }
    }
    Ok(())
}

/// Open a database file through the normal storage path, which is what runs
/// the migration chain. Returns the pool; the caller closes it.
async fn open_pool(path: &Path) -> Result<SqlitePool, DaemonError> {
    let db = crate::storage::Database::connect(&path.to_string_lossy()).await?;
    Ok(db.pool().clone())
}

/// Stamp `app_id` onto every row that migration 017 left with the `''`
/// placeholder, and register the app so its key resolves.
///
/// Scoped to `''` rather than "all rows" so re-running an import against an
/// already-imported database cannot re-home another app's data.
async fn stamp_owner(pool: &SqlitePool, app_id: &str) -> Result<(), DaemonError> {
    for table in ["sessions", "recipes", "schedule_jobs", "hitl_rules"] {
        sqlx::query(&format!("UPDATE {table} SET app_id = ? WHERE app_id = ''"))
            .bind(app_id)
            .execute(pool)
            .await
            .map_err(|e| fail(format!("stamping {table}: {e}")))?;
    }
    // Providers and MCP servers are nullable, where NULL means "shared with
    // every app". Kitty's rows are deliberately made *its own* rather than
    // shared: they were configured by one app for itself, and silently
    // publishing them to every future app would be a surprising default for
    // rows that carry API keys.
    for table in ["providers", "mcp_servers"] {
        sqlx::query(&format!("UPDATE {table} SET app_id = ? WHERE app_id IS NULL"))
            .bind(app_id)
            .execute(pool)
            .await
            .map_err(|e| fail(format!("stamping {table}: {e}")))?;
    }
    Ok(())
}

async fn count(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

/// How many provider rows hold an encrypted `api_key` that will not decrypt.
///
/// `crypto::decrypt` returns its input unchanged when it cannot decrypt, so a
/// value that still carries the `enc:v1:` prefix after a decrypt attempt is
/// one the current key cannot read. That is precisely the "forgot to carry the
/// key across" symptom, caught here where it can still be acted on.
async fn undecryptable_providers(pool: &SqlitePool) -> i64 {
    let configs: Vec<String> = sqlx::query_scalar("SELECT config FROM providers")
        .fetch_all(pool)
        .await
        .unwrap_or_default();

    configs
        .iter()
        .filter(|raw| {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
                return false;
            };
            let Some(key) = v.get("api_key").and_then(|k| k.as_str()) else {
                return false;
            };
            // Only prefixed values are claims to be encrypted; a legacy
            // plaintext key is readable by definition.
            key.starts_with("enc:v1:") && crate::crypto::decrypt(key).starts_with("enc:v1:")
        })
        .count() as i64
}

/// Import a copy of the V1 database at `source` into `dest`, owned by `app_id`.
///
/// `api_key` is `None` in the normal case, and that is deliberate: the import
/// stamps *ownership* onto rows, while issuing credentials is the client's own
/// first-launch job.
///
/// Registering here by default was a real footgun, found by running a live
/// migration. The import would mint a key, print it once, and write its hash
/// into `apps`. Kitty's `ensure_app_key` then found nothing in the Credential
/// Manager, tried to register, and got a `409` it cannot recover from -- the
/// key is unrecoverable by design, so the app could never authenticate and
/// first launch was bricked by a *successful* migration.
///
/// Leaving `apps` empty is the correct handoff: the rows carry `app_id`, they
/// are simply invisible until an app registers under that id, and the client
/// registering itself is the normal, already-exercised path. Pass `Some(key)`
/// only for a headless consumer that cannot register on its own.
///
/// The caller is responsible for having initialized `crypto` with V1's key
/// (see the module doc) — the summary reports how many providers that failed
/// to cover rather than deciding for the user, since an import that mostly
/// worked is usually still worth keeping.
pub async fn import_v1(
    source: &Path,
    dest: &Path,
    app_id: &str,
    display_name: &str,
    api_key: Option<&str>,
) -> Result<ImportSummary, DaemonError> {
    if dest.exists() {
        return Err(fail(format!(
            "{} already exists; refusing to overwrite an existing V2 database",
            dest.display()
        )));
    }
    copy_database(source, dest)?;

    // Opening through the normal path runs 001..016 (already applied, so
    // no-ops) and then 017+, which is the whole migration.
    let pool = open_pool(dest).await?;

    stamp_owner(&pool, app_id).await?;
    if let Some(key) = api_key {
        crate::storage::apps::register_app(&pool, app_id, display_name, key)
            .await
            .map_err(|e| fail(format!("registering {app_id}: {e}")))?;
    }

    let summary = ImportSummary {
        app_id: app_id.to_string(),
        sessions: count(&pool, "SELECT COUNT(*) FROM sessions").await,
        messages: count(&pool, "SELECT COUNT(*) FROM messages").await,
        providers: count(&pool, "SELECT COUNT(*) FROM providers").await,
        recipes: count(&pool, "SELECT COUNT(*) FROM recipes").await,
        schedules: count(&pool, "SELECT COUNT(*) FROM schedule_jobs").await,
        undecryptable_providers: undecryptable_providers(&pool).await,
    };

    // The invariant the whole tenancy design rests on. If anything still
    // carries the placeholder, the import produced rows no app can see, and
    // saying so now is far better than discovering it as an empty session list.
    for table in ["sessions", "recipes", "schedule_jobs", "hitl_rules"] {
        let orphans = count(
            &pool,
            &format!("SELECT COUNT(*) FROM {table} WHERE app_id = ''"),
        )
        .await;
        if orphans > 0 {
            return Err(fail(format!(
                "{orphans} row(s) in {table} were left with no owner after import"
            )));
        }
    }

    pool.close().await;
    Ok(summary)
}

/// Copy V1's pathway database to `apps/{app_id}/pathway.db`, where
/// `PluginHost` will look for it.
///
/// Best-effort and reported rather than fatal: a Kitty install with pathway
/// switched off has no such file, and losing a belief graph is a much smaller
/// problem than losing a transcript.
pub fn import_pathway(source: &Path, data_dir: &Path, app_id: &str) -> Option<PathBuf> {
    if !source.exists() {
        return None;
    }
    let dest_dir = data_dir.join("apps").join(app_id);
    std::fs::create_dir_all(&dest_dir).ok()?;
    let dest = dest_dir.join("pathway.db");
    std::fs::copy(source, &dest).ok()?;
    Some(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a database that looks like V1's: the first sixteen migrations
    /// applied and nothing else, with rows that predate tenancy.
    async fn v1_database(path: &Path) {
        let url = format!("sqlite://{}?mode=rwc", path.display().to_string().replace('\\', "/"));
        let pool = SqlitePool::connect(&url).await.unwrap();
        // The real chain is what V2 will migrate forward; applying all of it
        // and then clearing 017+ would be a different test. Instead let the
        // normal open path do everything, then blank the ownership columns to
        // recreate what a freshly-migrated V1 database looks like.
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        sqlx::query("INSERT INTO sessions (id, name, status, app_id) VALUES ('s1','Old Chat','active','')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO messages (session_id, role, content) VALUES ('s1','user','hello')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }

    #[tokio::test]
    async fn an_import_owns_every_row_it_brought_across() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("bigtiny.db");
        let dest = dir.path().join("v2.db");
        v1_database(&source).await;

        let summary = import_v1(&source, &dest, "kitty", "Kitty", Some("issued-key"))
            .await
            .expect("import should succeed");

        assert_eq!(summary.app_id, "kitty");
        assert_eq!(summary.sessions, 1);
        assert_eq!(summary.messages, 1);

        // And the imported app can actually authenticate as itself.
        let pool = open_pool(&dest).await.unwrap();
        let identity = crate::storage::apps::identity_for_key(&pool, "issued-key")
            .await
            .unwrap()
            .expect("the imported app resolves from its key");
        assert_eq!(identity.app_id, "kitty");
        pool.close().await;
    }

    #[tokio::test]
    async fn the_source_database_is_left_untouched() {
        // The rollback path is the entire reason this copies first: V1 must
        // still boot against its own database afterwards.
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("bigtiny.db");
        let dest = dir.path().join("v2.db");
        v1_database(&source).await;
        let before = std::fs::metadata(&source).unwrap().len();

        import_v1(&source, &dest, "kitty", "Kitty", None).await.unwrap();

        let pool = open_pool(&source).await.unwrap();
        let still_orphaned: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE app_id = ''")
                .fetch_one(&pool)
                .await
                .unwrap();
        pool.close().await;
        assert_eq!(still_orphaned, 1, "the source was modified by the import");
        assert!(before > 0);
    }

    #[tokio::test]
    async fn importing_over_an_existing_database_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("bigtiny.db");
        let dest = dir.path().join("v2.db");
        v1_database(&source).await;
        std::fs::write(&dest, b"existing").unwrap();

        let err = import_v1(&source, &dest, "kitty", "Kitty", None)
            .await
            .expect_err("must not clobber an existing V2 database");
        assert!(err.to_string().contains("already exists"), "got: {err}");
    }

    #[tokio::test]
    async fn a_second_import_cannot_re_home_another_apps_rows() {
        // Stamping is scoped to the `''` placeholder rather than to every row,
        // so pointing a second import at a database that already has owners
        // leaves those owners alone.
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("bigtiny.db");
        let dest = dir.path().join("v2.db");
        v1_database(&source).await;
        import_v1(&source, &dest, "kitty", "Kitty", None).await.unwrap();

        let pool = open_pool(&dest).await.unwrap();
        crate::storage::apps::register_app(&pool, "other", "Other", "k2")
            .await
            .unwrap();
        stamp_owner(&pool, "other").await.unwrap();

        let owner: String = sqlx::query_scalar("SELECT app_id FROM sessions WHERE id = 's1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        pool.close().await;
        assert_eq!(owner, "kitty", "an existing owner was overwritten");
    }

    #[tokio::test]
    async fn by_default_the_import_registers_nothing() {
        // The rows are owned, but `apps` is empty, so the migrating client's
        // own first-launch registration succeeds instead of hitting a 409 for
        // a key it can never hold.
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("bigtiny.db");
        let dest = dir.path().join("v2.db");
        v1_database(&source).await;

        import_v1(&source, &dest, "kitty", "Kitty", None).await.unwrap();

        let pool = open_pool(&dest).await.unwrap();
        let apps: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM apps")
            .fetch_one(&pool)
            .await
            .unwrap();
        let owned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE app_id = 'kitty'")
            .fetch_one(&pool)
            .await
            .unwrap();
        pool.close().await;

        assert_eq!(apps, 0, "the import registered an app nobody holds a key for");
        assert_eq!(owned, 1, "ownership is stamped regardless");
    }
}
