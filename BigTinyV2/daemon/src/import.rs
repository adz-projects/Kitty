//! Phase 7a: migrate a Kitty V1 database forward into V2.
//!
//! Kitty has real accumulated state — sessions, message history, providers,
//! schedules. Because the V2 fork kept V1's sixteen migrations intact
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
             {} providers, {} schedules",
            self.app_id, self.sessions, self.messages, self.providers, self.schedules,
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
        return Err(fail(format!("no database at {}", source.display())));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| fail(format!("could not create {}: {e}", parent.display())))?;
    }
    std::fs::copy(source, dest).map_err(|e| fail(format!("could not copy database: {e}")))?;

    for suffix in ["-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{suffix}", source.display()));
        if from.exists() {
            let to = PathBuf::from(format!("{}{suffix}", dest.display()));
            std::fs::copy(&from, &to)
                .map_err(|e| fail(format!("could not copy {}: {e}", from.display())))?;
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
    // No `recipes`: migration 021 dropped that table when recipes became
    // specialists, so a V1 database's recipe rows are gone by the time an
    // import runs. Nothing to stamp, and stamping would 500 on a missing table.
    for table in ["sessions", "schedule_jobs", "hitl_rules"] {
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
        sqlx::query(&format!(
            "UPDATE {table} SET app_id = ? WHERE app_id IS NULL"
        ))
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
        schedules: count(&pool, "SELECT COUNT(*) FROM schedule_jobs").await,
        undecryptable_providers: undecryptable_providers(&pool).await,
    };

    // The invariant the whole tenancy design rests on. If anything still
    // carries the placeholder, the import produced rows no app can see, and
    // saying so now is far better than discovering it as an empty session list.
    for table in ["sessions", "schedule_jobs", "hitl_rules"] {
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

// ---------------------------------------------------------------------------
// Merge import: V1 into a live V2 database
// ---------------------------------------------------------------------------

/// What a merge import brought across, for the client to report.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MergeSummary {
    pub sessions: i64,
    /// V1 sessions whose id the live database already had (an earlier import).
    pub sessions_skipped: i64,
    pub messages: i64,
    pub providers: i64,
    pub providers_skipped: i64,
    pub mcp_servers: i64,
    pub mcp_servers_skipped: i64,
    pub hitl_rules: i64,
    /// Provider keys or MCP header values that did not decrypt with the V1 key
    /// supplied. They were imported as they were and will fail to
    /// authenticate.
    pub undecryptable_secrets: i64,
    /// `imported`, `not_requested`, `not_found`, `skipped_not_empty`,
    /// `skipped_in_use` or `failed`. Filled in by the caller, which owns the
    /// live engine.
    pub pathway: String,
}

/// A merge's summary plus the new rows the running daemon still has to pick
/// up (the provider router and MCP manager hold their own in-memory view).
#[derive(Debug, Default)]
pub struct MergeOutcome {
    pub summary: MergeSummary,
    pub new_provider_ids: Vec<String>,
    pub new_mcp_server_ids: Vec<String>,
}

/// Merge a Kitty V1 database into the **live** V2 database as `app_id`'s data.
///
/// [`import_v1`] builds a fresh V2 database and refuses an existing one, which
/// suits a command-line migration done before V2 ever ran. A client that
/// finds V1 data after V2 is already in use needs this instead: nothing
/// already in the live database is touched, and anything the live database
/// already has (by id; MCP servers also by name) is skipped rather than
/// overwritten, so running it twice is harmless.
///
/// The source is never opened in place. It is copied to a staging file under
/// `data_dir`, migrated forward there (so its columns match), stamped with
/// `app_id`, and has its secrets re-encrypted from `v1_key` to this daemon's
/// key. Only then is it attached to one live connection and merged in a
/// single transaction; the staging copy is deleted afterwards.
///
/// Message rowids are shifted by a fixed offset past the live table's highest
/// rowid, rather than renumbered, so each imported session's
/// `compacted_through_rowid` can be shifted by the same offset and still
/// point at the same message.
pub async fn merge_v1(
    live: &SqlitePool,
    data_dir: &Path,
    source: &Path,
    v1_key: &[u8; 32],
    app_id: &str,
) -> Result<MergeOutcome, DaemonError> {
    let staging_dir = data_dir.join("import-staging");
    // Whatever an earlier import could not delete (see below).
    remove_staging(&staging_dir).await;
    let staging = staging_dir.join(format!("v1-{}.db", uuid::Uuid::new_v4()));
    let result = merge_via_staging(live, source, &staging, v1_key, app_id).await;
    remove_staging(&staging_dir).await;
    result
}

/// Delete the staging directory, retrying briefly: on Windows a just-closed
/// SQLite file can stay locked for a moment (the last handle, or an
/// antivirus scan of the new file). What still cannot be removed is left for
/// the next import to clear.
async fn remove_staging(dir: &Path) {
    for attempt in 0..20 {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) if attempt == 19 => {
                tracing::warn!("could not remove import staging {}: {e}", dir.display());
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
}

async fn merge_via_staging(
    live: &SqlitePool,
    source: &Path,
    staging: &Path,
    v1_key: &[u8; 32],
    app_id: &str,
) -> Result<MergeOutcome, DaemonError> {
    copy_database(source, staging)?;
    let undecryptable_secrets = {
        let pool = open_pool(staging).await?;
        let prepared = prepare_staging(&pool, v1_key, app_id).await;
        pool.close().await;
        prepared?
    };

    // ATTACH is per-connection and cannot run inside a transaction, so the
    // merge holds one pooled connection throughout. It must not go back to
    // the pool still attached (the next import's ATTACH would fail), so if
    // DETACH fails the connection is closed instead of returned.
    let mut conn = live
        .acquire()
        .await
        .map_err(|e| fail(format!("no database connection for the import: {e}")))?;
    sqlx::query("ATTACH DATABASE ? AS v1")
        .bind(read_only_uri(staging))
        .execute(&mut *conn)
        .await
        .map_err(|e| fail(format!("attaching the V1 copy: {e}")))?;

    let merged = merge_attached(&mut conn, app_id).await;
    if sqlx::query("DETACH DATABASE v1")
        .execute(&mut *conn)
        .await
        .is_err()
    {
        conn.close_on_drop();
    }
    drop(conn);
    let mut outcome = merged?;
    outcome.summary.undecryptable_secrets = undecryptable_secrets;
    Ok(outcome)
}

/// `path` as a read-only SQLite URI for `ATTACH`.
///
/// A URI rather than a plain path because an attached database inherits the
/// main connection's open flags: attached to an in-memory database, a plain
/// path opens as a new, empty in-memory database. `mode=ro` replaces those
/// flags, and read-only is what the merge wants anyway. sqlx always opens with
/// URI filenames enabled.
fn read_only_uri(path: &Path) -> String {
    let mut p = path.to_string_lossy().replace('\\', "/");
    if let Some(stripped) = p.strip_prefix("//?/") {
        p = stripped.to_string();
    }
    let p = p
        .replace('%', "%25")
        .replace('?', "%3f")
        .replace('#', "%23");
    let p = if p.starts_with('/') {
        p
    } else {
        format!("/{p}")
    };
    format!("file://{p}?mode=ro")
}

/// Stamp the staging copy with its owner and move its secrets onto this
/// daemon's key. Returns how many secrets would not decrypt.
async fn prepare_staging(
    pool: &SqlitePool,
    v1_key: &[u8; 32],
    app_id: &str,
) -> Result<i64, DaemonError> {
    stamp_owner(pool, app_id).await?;
    for table in ["sessions", "hitl_rules"] {
        let orphans = count(
            pool,
            &format!("SELECT COUNT(*) FROM {table} WHERE app_id = ''"),
        )
        .await;
        if orphans > 0 {
            return Err(fail(format!(
                "{orphans} row(s) in {table} were left with no owner"
            )));
        }
    }

    let mut undecryptable = 0i64;
    // Re-encrypt one secret: V1-encrypted values are opened with V1's key and
    // sealed with ours; legacy plaintext is sealed too. `None` = no change.
    let mut reseal = |value: &str| -> Option<String> {
        if value.is_empty() {
            return None;
        }
        match crate::crypto::decrypt_with_key(value, v1_key) {
            Some(plain) => Some(crate::crypto::encrypt(&plain)),
            None => {
                undecryptable += 1;
                None
            }
        }
    };

    let providers: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT id, config FROM providers")
            .fetch_all(pool)
            .await
            .map_err(|e| fail(format!("reading V1 providers: {e}")))?;
    for (id, config) in providers {
        let Some(mut config) = config
            .as_deref()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok())
        else {
            continue;
        };
        let Some(sealed) = config
            .get("api_key")
            .and_then(|k| k.as_str())
            .and_then(&mut reseal)
        else {
            continue;
        };
        config["api_key"] = serde_json::Value::String(sealed);
        sqlx::query("UPDATE providers SET config = ? WHERE id = ?")
            .bind(config.to_string())
            .bind(&id)
            .execute(pool)
            .await
            .map_err(|e| fail(format!("re-encrypting provider {id}: {e}")))?;
    }

    let servers: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT id, headers FROM mcp_servers")
            .fetch_all(pool)
            .await
            .map_err(|e| fail(format!("reading V1 MCP servers: {e}")))?;
    for (id, headers) in servers {
        let Some(serde_json::Value::Object(mut headers)) = headers
            .as_deref()
            .and_then(|h| serde_json::from_str::<serde_json::Value>(h).ok())
        else {
            continue;
        };
        let mut changed = false;
        for value in headers.values_mut() {
            if let Some(sealed) = value.as_str().and_then(&mut reseal) {
                *value = serde_json::Value::String(sealed);
                changed = true;
            }
        }
        if changed {
            sqlx::query("UPDATE mcp_servers SET headers = ? WHERE id = ?")
                .bind(serde_json::Value::Object(headers).to_string())
                .bind(&id)
                .execute(pool)
                .await
                .map_err(|e| fail(format!("re-encrypting MCP server {id}: {e}")))?;
        }
    }
    Ok(undecryptable)
}

/// The merge proper, against a connection with the prepared copy attached as
/// `v1`. All or nothing.
async fn merge_attached(
    conn: &mut sqlx::SqliteConnection,
    app_id: &str,
) -> Result<MergeOutcome, DaemonError> {
    use sqlx::Connection;

    let step = |what: &'static str| move |e: sqlx::Error| fail(format!("{what}: {e}"));
    let mut tx = conn.begin().await.map_err(step("starting the import"))?;

    sqlx::query("DROP TABLE IF EXISTS temp.import_sessions")
        .execute(&mut *tx)
        .await
        .map_err(step("preparing"))?;
    sqlx::query(
        "CREATE TEMP TABLE import_sessions AS \
         SELECT id FROM v1.sessions WHERE app_id = ? AND id NOT IN (SELECT id FROM main.sessions)",
    )
    .bind(app_id)
    .execute(&mut *tx)
    .await
    .map_err(step("selecting sessions"))?;
    let v1_sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM v1.sessions")
        .fetch_one(&mut *tx)
        .await
        .map_err(step("counting sessions"))?;
    let offset: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(rowid), 0) FROM main.messages")
        .fetch_one(&mut *tx)
        .await
        .map_err(step("reading the message offset"))?;

    // A compaction that was running when V1 stopped is not running now.
    let sessions = sqlx::query(
        "INSERT INTO main.sessions (id, name, created_at, updated_at, status, metadata, \
             memory_slots, compacted_through_rowid, compaction_state, compaction_started_at, \
             app_id, parent_session_id) \
         SELECT id, name, created_at, updated_at, status, metadata, memory_slots, \
             CASE WHEN compacted_through_rowid > 0 THEN compacted_through_rowid + ?1 ELSE 0 END, \
             'idle', NULL, app_id, parent_session_id \
         FROM v1.sessions WHERE id IN (SELECT id FROM temp.import_sessions)",
    )
    .bind(offset)
    .execute(&mut *tx)
    .await
    .map_err(step("importing sessions"))?
    .rows_affected() as i64;

    let messages = sqlx::query(
        "INSERT OR IGNORE INTO main.messages (rowid, id, session_id, role, content, tool_calls, \
             token_count, created_at, tool_call_id, content_format, reasoning, provider_id, model) \
         SELECT rowid + ?1, id, session_id, role, content, tool_calls, token_count, created_at, \
             tool_call_id, content_format, reasoning, provider_id, model \
         FROM v1.messages WHERE session_id IN (SELECT id FROM temp.import_sessions) \
         ORDER BY rowid",
    )
    .bind(offset)
    .execute(&mut *tx)
    .await
    .map_err(step("importing messages"))?
    .rows_affected() as i64;

    sqlx::query(
        "INSERT OR IGNORE INTO main.llm_timings (id, session_id, provider_id, model, ttfb_ms, \
             ttft_ms, generation_ms, total_tokens, created_at) \
         SELECT id, session_id, provider_id, model, ttfb_ms, ttft_ms, generation_ms, \
             total_tokens, created_at \
         FROM v1.llm_timings WHERE session_id IN (SELECT id FROM temp.import_sessions)",
    )
    .execute(&mut *tx)
    .await
    .map_err(step("importing timings"))?;

    let new_provider_ids: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM v1.providers WHERE id NOT IN (SELECT id FROM main.providers)",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(step("selecting providers"))?;
    let v1_providers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM v1.providers")
        .fetch_one(&mut *tx)
        .await
        .map_err(step("counting providers"))?;
    sqlx::query(
        "INSERT INTO main.providers (id, name, provider_type, base_url, fallback_priority, \
             config, status, error_message, created_at, updated_at, app_id) \
         SELECT id, name, provider_type, base_url, fallback_priority, config, 'disconnected', \
             NULL, created_at, updated_at, app_id \
         FROM v1.providers WHERE id NOT IN (SELECT id FROM main.providers)",
    )
    .execute(&mut *tx)
    .await
    .map_err(step("importing providers"))?;

    // By name as well as id: the client has usually registered its own
    // built-in servers under the same names by now, and a second row per
    // name would double every tool.
    let new_mcp_server_ids: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM v1.mcp_servers \
         WHERE id NOT IN (SELECT id FROM main.mcp_servers) \
           AND name NOT IN (SELECT name FROM main.mcp_servers WHERE app_id = ?1 OR app_id IS NULL)",
    )
    .bind(app_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(step("selecting MCP servers"))?;
    let v1_servers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM v1.mcp_servers")
        .fetch_one(&mut *tx)
        .await
        .map_err(step("counting MCP servers"))?;
    for id in &new_mcp_server_ids {
        sqlx::query(
            "INSERT INTO main.mcp_servers (id, name, transport, command, args, url, env, \
                 headers, enabled, status, error_message, created_at, updated_at, timeout_s, \
                 app_id) \
             SELECT id, name, transport, command, args, url, env, headers, enabled, \
                 'disconnected', NULL, created_at, updated_at, timeout_s, app_id \
             FROM v1.mcp_servers WHERE id = ?",
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(step("importing MCP servers"))?;
    }

    let hitl_rules = sqlx::query(
        "INSERT INTO main.hitl_rules (tool_name, args_pattern, decision, created_at, app_id) \
         SELECT r.tool_name, r.args_pattern, r.decision, r.created_at, r.app_id \
         FROM v1.hitl_rules r WHERE r.app_id = ?1 AND NOT EXISTS ( \
             SELECT 1 FROM main.hitl_rules m WHERE m.app_id = r.app_id \
               AND m.tool_name = r.tool_name AND m.args_pattern IS r.args_pattern \
               AND m.decision = r.decision)",
    )
    .bind(app_id)
    .execute(&mut *tx)
    .await
    .map_err(step("importing approval rules"))?
    .rows_affected() as i64;

    sqlx::query("DROP TABLE temp.import_sessions")
        .execute(&mut *tx)
        .await
        .map_err(step("cleaning up"))?;
    tx.commit().await.map_err(step("committing the import"))?;

    Ok(MergeOutcome {
        summary: MergeSummary {
            sessions,
            sessions_skipped: v1_sessions - sessions,
            messages,
            providers: new_provider_ids.len() as i64,
            providers_skipped: v1_providers - new_provider_ids.len() as i64,
            mcp_servers: new_mcp_server_ids.len() as i64,
            mcp_servers_skipped: v1_servers - new_mcp_server_ids.len() as i64,
            hitl_rules,
            undecryptable_secrets: 0,
            pathway: String::new(),
        },
        new_provider_ids,
        new_mcp_server_ids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a database that looks like V1's: the first sixteen migrations
    /// applied and nothing else, with rows that predate tenancy.
    async fn v1_database(path: &Path) {
        let url = format!(
            "sqlite://{}?mode=rwc",
            path.display().to_string().replace('\\', "/")
        );
        let pool = SqlitePool::connect(&url).await.unwrap();
        // The real chain is what V2 will migrate forward; applying all of it
        // and then clearing 017+ would be a different test. Instead let the
        // normal open path do everything, then blank the ownership columns to
        // recreate what a freshly-migrated V1 database looks like.
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO sessions (id, name, status, app_id) VALUES ('s1','Old Chat','active','')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO messages (session_id, role, content) VALUES ('s1','user','hello')",
        )
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

        import_v1(&source, &dest, "kitty", "Kitty", None)
            .await
            .unwrap();

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
        import_v1(&source, &dest, "kitty", "Kitty", None)
            .await
            .unwrap();

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

        import_v1(&source, &dest, "kitty", "Kitty", None)
            .await
            .unwrap();

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

        assert_eq!(
            apps, 0,
            "the import registered an app nobody holds a key for"
        );
        assert_eq!(owned, 1, "ownership is stamped regardless");
    }

    /// A live V2 database with one session and one message of its own.
    async fn live_database(dir: &Path) -> SqlitePool {
        let pool = open_pool(&dir.join("live.db")).await.unwrap();
        sqlx::query("INSERT INTO sessions (id, name, status, app_id) VALUES ('live1','Live','active','kitty')")
            .execute(&pool)
            .await
            .unwrap();
        for i in 0..3 {
            sqlx::query("INSERT INTO messages (id, session_id, role, content) VALUES (?, 'live1', 'user', 'x')")
                .bind(format!("lm{i}"))
                .execute(&pool)
                .await
                .unwrap();
        }
        pool
    }

    /// A V1 database with a compacted session, a provider whose key is sealed
    /// under V1's key, an MCP server, and an approval rule.
    async fn rich_v1_database(path: &Path, v1_key: &[u8; 32]) {
        v1_database(path).await;
        let url = format!(
            "sqlite://{}?mode=rwc",
            path.display().to_string().replace('\\', "/")
        );
        let pool = SqlitePool::connect(&url).await.unwrap();
        for (id, content) in [("m2", "second"), ("m3", "third")] {
            sqlx::query("INSERT INTO messages (id, session_id, role, content) VALUES (?, 's1', 'assistant', ?)")
                .bind(id)
                .bind(content)
                .execute(&pool)
                .await
                .unwrap();
        }
        // Compacted through "second".
        sqlx::query(
            "UPDATE sessions SET compacted_through_rowid = (SELECT rowid FROM messages WHERE id = 'm2') WHERE id = 's1'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let config = serde_json::json!({
            "api_key": crate::crypto::encrypt_with_key("sk-from-v1", v1_key),
            "models": ["m"],
        });
        sqlx::query(
            "INSERT INTO providers (id, name, provider_type, base_url, config) \
             VALUES ('p1', 'OpenRouter', 'openai_compat', 'https://x', ?)",
        )
        .bind(config.to_string())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO mcp_servers (id, name, transport, command) VALUES ('mcp1', 'custom', 'stdio', 'x.exe'), \
             ('mcp2', 'kitty-tools', 'stdio', 'old.exe')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO hitl_rules (tool_name, decision, app_id) VALUES ('lean_shell', 'always_allow', '')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }

    #[tokio::test]
    async fn a_merge_brings_v1_into_a_live_database_without_touching_it() {
        let dir = tempfile::tempdir().unwrap();
        let live = live_database(dir.path()).await;
        // The client's own built-in, registered before the import runs.
        sqlx::query("INSERT INTO mcp_servers (id, name, transport, command, app_id) VALUES ('mine', 'kitty-tools', 'stdio', 'new.exe', 'kitty')")
            .execute(&live)
            .await
            .unwrap();
        let v1_key = [11u8; 32];
        let source = dir.path().join("bigtiny.db");
        rich_v1_database(&source, &v1_key).await;

        let outcome = merge_v1(&live, dir.path(), &source, &v1_key, "kitty")
            .await
            .expect("merge succeeds");
        let s = &outcome.summary;
        assert_eq!((s.sessions, s.messages), (1, 3));
        assert_eq!((s.providers, s.mcp_servers, s.hitl_rules), (1, 1, 1));
        assert_eq!(
            s.mcp_servers_skipped, 1,
            "the client's own kitty-tools is kept"
        );
        assert_eq!(s.undecryptable_secrets, 0);
        assert_eq!(outcome.new_provider_ids, ["p1"]);
        assert_eq!(outcome.new_mcp_server_ids, ["mcp1"]);

        // The live rows are untouched and the imported ones are the caller's.
        let owners: Vec<String> = sqlx::query_scalar("SELECT app_id FROM sessions ORDER BY id")
            .fetch_all(&live)
            .await
            .unwrap();
        assert_eq!(owners, ["kitty", "kitty"]);
        let command: String =
            sqlx::query_scalar("SELECT command FROM mcp_servers WHERE id = 'mine'")
                .fetch_one(&live)
                .await
                .unwrap();
        assert_eq!(command, "new.exe");

        // The compaction pointer still names the same message.
        let compacted: String = sqlx::query_scalar(
            "SELECT m.id FROM messages m JOIN sessions s ON m.rowid = s.compacted_through_rowid WHERE s.id = 's1'",
        )
        .fetch_one(&live)
        .await
        .unwrap();
        assert_eq!(compacted, "m2");

        // The provider key now opens with this daemon's key.
        let config: String = sqlx::query_scalar("SELECT config FROM providers WHERE id = 'p1'")
            .fetch_one(&live)
            .await
            .unwrap();
        let config: serde_json::Value = serde_json::from_str(&config).unwrap();
        assert_eq!(
            crate::crypto::decrypt(config["api_key"].as_str().unwrap()),
            "sk-from-v1"
        );

        // The imported history is searchable like any other.
        let hits: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'third'",
        )
        .fetch_one(&live)
        .await
        .unwrap();
        assert_eq!(hits, 1);

        // Nothing is left behind, and a second run brings nothing new.
        assert!(!dir.path().join("import-staging").exists());
        let again = merge_v1(&live, dir.path(), &source, &v1_key, "kitty")
            .await
            .unwrap();
        assert_eq!(
            (
                again.summary.sessions,
                again.summary.messages,
                again.summary.providers,
                again.summary.hitl_rules
            ),
            (0, 0, 0, 0)
        );
        assert_eq!(again.summary.sessions_skipped, 1);
        live.close().await;
    }

    #[tokio::test]
    async fn a_wrong_v1_key_is_reported_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let live = live_database(dir.path()).await;
        let source = dir.path().join("bigtiny.db");
        rich_v1_database(&source, &[11u8; 32]).await;

        let outcome = merge_v1(&live, dir.path(), &source, &[12u8; 32], "kitty")
            .await
            .unwrap();
        assert_eq!(outcome.summary.undecryptable_secrets, 1);
        assert_eq!(
            outcome.summary.sessions, 1,
            "the history still comes across"
        );
        live.close().await;
    }
}
