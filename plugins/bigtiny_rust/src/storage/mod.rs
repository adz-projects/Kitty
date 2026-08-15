pub mod execution;
pub mod hitl_rules;
pub mod mcp_servers;
pub mod messages;
pub mod providers;
pub mod recipes;
pub mod schedules;
pub mod sessions;
pub mod timings;

use sqlx::migrate::Migrate;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::SqlitePool;
use std::str::FromStr;

use crate::error::StorageError;

pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// `path == "sqlite::memory:"` (or any other in-memory sqlite URL) is
    /// passed straight through — `SqliteConnectOptions::from_str` handles
    /// that form itself. A real file path is *not* auto-created by sqlx by
    /// default (`create_if_missing` is `false` unless set explicitly), so a
    /// first-ever run against a fresh data directory would otherwise fail
    /// with "unable to open database file" — this only ever went unnoticed
    /// because every test in this crate uses an in-memory DB.
    pub async fn connect(path: &str) -> Result<Self, StorageError> {
        let options = if path.starts_with("sqlite:") || path.contains("::memory:") {
            SqliteConnectOptions::from_str(path)
                .map_err(|e| StorageError::Generic(e.to_string()))?
        } else {
            if let Some(parent) = std::path::Path::new(path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        StorageError::Generic(format!(
                            "failed to create {}: {}",
                            parent.display(),
                            e
                        ))
                    })?;
                }
            }
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true)
        };

        // `foreign_keys` is a *per-connection* pragma, so the previous
        // `PRAGMA foreign_keys = ON` run via `execute(&pool)` only ever
        // reached whichever single connection the pool handed out — it was
        // never what actually kept 012's `ON DELETE CASCADE` working. What
        // does is sqlx's own default, which registers the pragma on every
        // connection it opens (`SqliteConnectOptions::default`). Stating it
        // here is redundant with that default but load-bearing as
        // documentation: the cascade delete depends on it, and a future
        // options rewrite that drops it would break session deletion in a way
        // that only shows up at runtime.
        //
        // `journal_mode = WAL` stays a one-shot below: it is a persistent
        // database-level property, not a per-connection one.
        let options = options.foreign_keys(true);

        let pool = SqlitePool::connect_with(options).await?;
        sqlx::query("PRAGMA journal_mode = WAL")
            .execute(&pool)
            .await?;
        bootstrap_legacy_python_schema(&pool).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

/// BigTiny's Python daemon (`plugins/bigtiny/bigtiny/storage.py`) tracks its
/// own hand-rolled `schema_version` table and has never heard of sqlx's
/// `_sqlx_migrations` bookkeeping table. Opening an existing Python-created
/// `bigtiny.db` with a blind `sqlx::migrate!(...).run()` would try to
/// re-apply every migration starting from version 1 — and several of them
/// are plain `ALTER TABLE ... ADD COLUMN` statements, which are not
/// idempotent, so the very first one against a column that already exists
/// fails with "duplicate column name" and the daemon refuses to start
/// against any pre-existing user database (confirmed against a real
/// Python-initialized `bigtiny.db`, which was sitting at `schema_version`
/// max version 5 — not even fully caught up to this crate's migration set).
///
/// If this looks like a legacy Python-initialized database (a
/// `schema_version` table exists, sqlx's own bookkeeping table doesn't yet),
/// this pre-seeds `_sqlx_migrations` with synthetic "already applied" rows
/// for every version already recorded in `schema_version`, using this
/// crate's own compile-time-embedded migration checksums — so the
/// `sqlx::migrate!(...).run()` call right after this correctly treats those
/// versions as already done (checksums match, since the SQL is identical —
/// this crate's migrations were ported 1:1 from the Python ones) and only
/// applies whatever versions come after. A no-op for a fresh database or one
/// this daemon has already opened before.
async fn bootstrap_legacy_python_schema(pool: &SqlitePool) -> Result<(), StorageError> {
    let has_legacy_table: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_version'",
    )
    .fetch_one(pool)
    .await?;
    if has_legacy_table == 0 {
        return Ok(());
    }

    let already_bootstrapped: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_one(pool)
    .await?;
    if already_bootstrapped > 0 {
        return Ok(());
    }

    let max_version: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM schema_version")
            .fetch_one(pool)
            .await?;
    if max_version == 0 {
        return Ok(());
    }

    let mut conn = pool.acquire().await?;
    conn.ensure_migrations_table()
        .await
        .map_err(|e| StorageError::Generic(e.to_string()))?;

    let migrator = sqlx::migrate!("./migrations");
    for m in migrator.iter().filter(|m| m.version <= max_version) {
        sqlx::query(
            "INSERT OR IGNORE INTO _sqlx_migrations \
             (version, description, success, checksum, execution_time) \
             VALUES (?1, ?2, TRUE, ?3, -1)",
        )
        .bind(m.version)
        .bind(m.description.as_ref())
        .bind(m.checksum.as_ref())
        .execute(&mut *conn)
        .await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::SqlitePool;

    use super::{
        execution, hitl_rules, mcp_servers, messages, providers, recipes, schedules, sessions,
        timings,
    };

    async fn get_test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("PRAGMA journal_mode = WAL")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    /// Regression test for the "unable to open database file" bug: connect
    /// to a real file path under a data directory that doesn't exist yet
    /// (matching a fresh install's first run) rather than `sqlite::memory:`.
    #[tokio::test]
    async fn connect_creates_missing_parent_dir_and_db_file() {
        let dir = std::env::temp_dir().join(format!(
            "bigtiny-rust-connect-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let db_path = dir.join("bigtiny.db");
        assert!(!dir.exists());

        let db = super::Database::connect(db_path.to_str().unwrap())
            .await
            .unwrap();
        assert!(db_path.exists());

        // The connection is actually usable, not just openable.
        let session = sessions::create_session(db.pool(), "s1", "Test")
            .await
            .unwrap();
        assert_eq!(session.id, "s1");

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Simulates a real-world Python-initialized `bigtiny.db`: hand-applies
    /// the Python daemon's V001-V003 SQL verbatim (`plugins/bigtiny/bigtiny/storage.py`)
    /// against a fresh file, including its own `schema_version` bookkeeping
    /// rows — deliberately stopping short of the full migration set, mirroring
    /// a real installed database found sitting at an intermediate version
    /// (schema_version max 5 out of 8) rather than assuming every user is
    /// fully caught up.
    async fn legacy_python_db_at_v003(db_path: &std::path::Path) {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(options).await.unwrap();
        sqlx::query(
            r#"
            CREATE TABLE schema_version (
                version INTEGER PRIMARY KEY,
                applied_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(
            r#"
            CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                name TEXT,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                status TEXT DEFAULT 'active' CHECK(status IN ('active', 'idle', 'failed', 'archived')),
                metadata TEXT
            );
            CREATE TABLE messages (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                role TEXT NOT NULL CHECK(role IN ('user', 'assistant', 'system', 'tool')),
                content TEXT,
                tool_calls TEXT,
                token_count INTEGER DEFAULT 0,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE hitl_rules (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                tool_name TEXT NOT NULL,
                args_pattern TEXT,
                decision TEXT NOT NULL CHECK(decision IN ('allow', 'always_allow', 'reject')),
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE providers (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                provider_type TEXT NOT NULL CHECK(provider_type IN ('openai_compat', 'anthropic')),
                base_url TEXT NOT NULL,
                fallback_priority INTEGER DEFAULT 1,
                config TEXT,
                status TEXT DEFAULT 'disconnected' CHECK(status IN ('connected', 'disconnected', 'error')),
                error_message TEXT,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE mcp_servers (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                transport TEXT NOT NULL CHECK(transport IN ('stdio', 'sse', 'streamable_http', 'in_process')),
                command TEXT,
                args TEXT,
                sse_url TEXT,
                env TEXT,
                status TEXT DEFAULT 'disconnected' CHECK(status IN ('connected', 'disconnected', 'error')),
                error_message TEXT,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE recipes (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                prompt_template TEXT NOT NULL,
                instructions TEXT,
                parameters TEXT,
                required_mcp_servers TEXT,
                system_prompt_layer TEXT,
                max_steps INTEGER DEFAULT 30,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE schedule_jobs (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                cron TEXT NOT NULL,
                recipe_id TEXT NOT NULL REFERENCES recipes(id),
                parameters TEXT,
                enabled INTEGER DEFAULT 1,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE execution_history (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id),
                trigger_type TEXT NOT NULL CHECK(trigger_type IN ('manual', 'schedule', 'recipe', 'subagent')),
                trigger_id TEXT,
                status TEXT NOT NULL CHECK(status IN ('running', 'completed', 'failed', 'cancelled')),
                started_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                completed_at TIMESTAMP,
                result_summary TEXT,
                error_message TEXT
            );
            CREATE INDEX idx_messages_session ON messages(session_id);
            CREATE INDEX idx_execution_session ON execution_history(session_id);
            ALTER TABLE messages ADD COLUMN tool_call_id TEXT;
            ALTER TABLE messages ADD COLUMN content_format TEXT DEFAULT 'text';
            INSERT INTO schema_version (version) VALUES (1), (2), (3);
            INSERT INTO sessions (id, name) VALUES ('legacy-sess', 'Legacy Session');
            INSERT INTO messages (id, session_id, role, content) VALUES ('legacy-msg', 'legacy-sess', 'user', 'hello from python');
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
    }

    #[tokio::test]
    async fn connect_opens_a_legacy_python_initialized_db_without_erroring() {
        let dir = std::env::temp_dir().join(format!(
            "bigtiny-rust-legacy-db-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("bigtiny.db");

        legacy_python_db_at_v003(&db_path).await;

        // The real regression: this used to fail with "duplicate column
        // name: tool_call_id" (migration 002 re-applied blindly against a
        // column the legacy Python schema already added).
        let db = super::Database::connect(db_path.to_str().unwrap())
            .await
            .expect("connect must succeed against a legacy Python-initialized db");

        // Pre-existing data survived untouched.
        let session = sessions::get_session(db.pool(), "legacy-sess")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.name, Some("Legacy Session".into()));
        let msgs = messages::get_messages_by_session(db.pool(), "legacy-sess")
            .await
            .unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content.as_deref(), Some("hello from python"));

        // And migrations past the legacy database's version (4 onward) were
        // actually applied — e.g. mcp_servers now has the `enabled`/`url`
        // columns from V004/V005, and later tables like `llm_timings` exist.
        mcp_servers::create_server(db.pool(), "srv-1", "test", "stdio")
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='llm_timings'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(
            count, 1,
            "expected V008's llm_timings table to have been applied"
        );

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Connecting a second time (the daemon restarting against a db it — or
    /// the bootstrap shim — already fully migrated) must stay a no-op, not
    /// re-run the bootstrap or error on an already-populated `_sqlx_migrations`.
    #[tokio::test]
    async fn connect_is_idempotent_across_repeat_opens_of_a_legacy_db() {
        let dir = std::env::temp_dir().join(format!(
            "bigtiny-rust-legacy-db-reopen-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("bigtiny.db");

        legacy_python_db_at_v003(&db_path).await;

        let db1 = super::Database::connect(db_path.to_str().unwrap())
            .await
            .unwrap();
        drop(db1);
        let db2 = super::Database::connect(db_path.to_str().unwrap())
            .await
            .expect("reconnecting to an already-bootstrapped db must succeed");
        let session = sessions::get_session(db2.pool(), "legacy-sess")
            .await
            .unwrap();
        assert!(session.is_some());

        drop(db2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_migrations_apply() {
        let pool = get_test_pool().await;
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sqlite_master WHERE type='table'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            count.0 >= 11,
            "Expected at least 11 tables, got {}",
            count.0
        );
    }

    #[tokio::test]
    async fn test_session_crud() {
        let pool = get_test_pool().await;

        let created = sessions::create_session(&pool, "test-session-1", "Test Session")
            .await
            .unwrap();
        assert_eq!(created.id, "test-session-1");
        assert_eq!(created.name, Some("Test Session".into()));
        assert_eq!(created.status, "active");

        let got = sessions::get_session(&pool, "test-session-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.id, "test-session-1");

        let list = sessions::list_sessions(&pool).await.unwrap();
        assert_eq!(list.len(), 1);

        sessions::update_session_name(&pool, "test-session-1", "Renamed Session")
            .await
            .unwrap();
        let updated = sessions::get_session(&pool, "test-session-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.name, Some("Renamed Session".into()));

        sessions::update_session_status(&pool, "test-session-1", "idle")
            .await
            .unwrap();
        let updated = sessions::get_session(&pool, "test-session-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, "idle");

        sessions::update_session_config(&pool, "test-session-1", "{\"key\": \"value\"}")
            .await
            .unwrap();
        let meta = sessions::get_session_metadata(&pool, "test-session-1")
            .await
            .unwrap();
        assert_eq!(meta, Some("{\"key\": \"value\"}".into()));

        let deleted = sessions::delete_session(&pool, "test-session-1")
            .await
            .unwrap();
        assert_eq!(deleted, 1);
        let gone = sessions::get_session(&pool, "test-session-1")
            .await
            .unwrap();
        assert!(gone.is_none());
    }

    #[tokio::test]
    async fn test_delete_session_cascades_execution_history() {
        // Migration 012 added ON DELETE CASCADE to `execution_history.session_id`
        // — deleting a session that has execution-history rows used to raise
        // SQLITE_CONSTRAINT_FOREIGNKEY (500 on DELETE /api/chat/{id}).
        let pool = get_test_pool().await;
        sessions::create_session(&pool, "cascade-s", "Cascade Test")
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO execution_history (id, session_id, trigger_type, status) \
             VALUES ('e1', 'cascade-s', 'manual', 'completed')",
        )
        .execute(&pool)
        .await
        .unwrap();

        sessions::delete_session(&pool, "cascade-s").await.unwrap();

        let leftover: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM execution_history WHERE id = 'e1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(leftover, 0);
    }

    #[tokio::test]
    async fn test_update_indexed_message_reindexes() {
        // Migration 013, update half. `messages_fts_au_del` used the same
        // invalid fts5 `'delete'` command as the delete trigger, so editing an
        // indexed message raised instead of reindexing.
        let pool = get_test_pool().await;
        sessions::create_session(&pool, "fts-u", "FTS Update")
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO messages (id, session_id, role, content) \
             VALUES ('m-u', 'fts-u', 'user', 'before')",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("UPDATE messages SET content = 'after' WHERE id = 'm-u'")
            .execute(&pool)
            .await
            .unwrap();

        // The index followed the edit rather than keeping the stale text.
        let hits: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'after'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(hits, 1);
        let stale: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'before'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(stale, 0);
    }

    #[tokio::test]
    async fn test_delete_session_with_system_messages() {
        // Migration 013. Two stacked defects, either of which fails the whole
        // cascaded DELETE:
        //
        //  - the removal triggers used fts5's `'delete'` command, which is
        //    only valid for external-content/contentless tables — and
        //    `messages_fts` is an ordinary one. So deleting any *indexed*
        //    message raised, which is most real conversations.
        //  - `messages_fts_ad` also lacked the `WHEN role != 'system' AND
        //    content IS NOT NULL` guard its insert sibling has.
        //
        // This session carries all three row shapes at once: indexed, system,
        // and NULL-content.
        let pool = get_test_pool().await;
        sessions::create_session(&pool, "fts-s", "FTS Test")
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO messages (id, session_id, role, content) VALUES \
             ('m-sys', 'fts-s', 'system', 'you are a helpful assistant'), \
             ('m-usr', 'fts-s', 'user', 'hello'), \
             ('m-null', 'fts-s', 'assistant', NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let deleted = sessions::delete_session(&pool, "fts-s").await.unwrap();
        assert_eq!(deleted, 1);

        let leftover: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE session_id = 'fts-s'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(leftover, 0);

        // The indexed row is gone from the FTS table too — the guard must not
        // have been widened into "never delete anything from the index".
        let indexed: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM messages_fts WHERE session_id = 'fts-s'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(indexed, 0);
    }

    #[tokio::test]
    async fn test_delete_recipe_cascades_schedule_jobs() {
        // Migration 012 added ON DELETE CASCADE to `schedule_jobs.recipe_id` —
        // deleting a still-referenced recipe used to 500.
        let pool = get_test_pool().await;
        sqlx::query("INSERT INTO recipes (id, name, prompt_template, max_steps) VALUES ('r-cascade', 'R', 'p', 10)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO schedule_jobs (id, name, cron, recipe_id, enabled) \
             VALUES ('s-cascade', 'job', '0 9 * * *', 'r-cascade', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("DELETE FROM recipes WHERE id = 'r-cascade'")
            .execute(&pool)
            .await
            .unwrap();

        let leftover: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM schedule_jobs WHERE id = 's-cascade'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(leftover, 0);
    }

    #[tokio::test]
    async fn test_compaction_lock_cas_and_stale_reclaim() {
        let pool = get_test_pool().await;
        sessions::create_session(&pool, "compact-1", "Compaction Test")
            .await
            .unwrap();

        // First acquire succeeds.
        let acquired = sessions::try_acquire_compaction_lock(
            &pool,
            "compact-1",
            chrono::Duration::seconds(60),
        )
        .await
        .unwrap();
        assert!(acquired);

        // A second acquire while still held (not stale) fails — this is the
        // race the lock exists to prevent.
        let second = sessions::try_acquire_compaction_lock(
            &pool,
            "compact-1",
            chrono::Duration::seconds(60),
        )
        .await
        .unwrap();
        assert!(!second);

        // Release, then it can be acquired again.
        sessions::release_compaction_lock(&pool, "compact-1")
            .await
            .unwrap();
        let after_release = sessions::try_acquire_compaction_lock(
            &pool,
            "compact-1",
            chrono::Duration::seconds(60),
        )
        .await
        .unwrap();
        assert!(after_release);

        // A lock held "forever" (stale_after=0) is reclaimable once it's at
        // least one second old — simulates recovering from a crashed
        // compaction pass. SQLite's CURRENT_TIMESTAMP has 1s resolution, so
        // sleep past that to avoid a same-second false negative.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let stale_reclaim =
            sessions::try_acquire_compaction_lock(&pool, "compact-1", chrono::Duration::seconds(0))
                .await
                .unwrap();
        assert!(stale_reclaim);
    }

    #[tokio::test]
    async fn test_message_crud() {
        let pool = get_test_pool().await;
        sessions::create_session(&pool, "msg-session-1", "Messages Test")
            .await
            .unwrap();

        let msgs = vec![
            messages::MessageRow {
                rowid: 0,
                id: "msg-1".into(),
                session_id: "msg-session-1".into(),
                role: "user".into(),
                content: Some("Hello".into()),
                tool_calls: None,
                tool_call_id: None,
                token_count: Some(3),
                content_format: Some("text".into()),
                created_at: None,
            },
            messages::MessageRow {
                rowid: 0,
                id: "msg-2".into(),
                session_id: "msg-session-1".into(),
                role: "assistant".into(),
                content: Some("Hi there!".into()),
                tool_calls: None,
                tool_call_id: None,
                token_count: Some(4),
                content_format: Some("text".into()),
                created_at: None,
            },
        ];
        messages::save_messages(&pool, "msg-session-1", &msgs)
            .await
            .unwrap();

        let got = messages::get_messages_by_session(&pool, "msg-session-1")
            .await
            .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "msg-1");
        assert_eq!(got[1].id, "msg-2");

        messages::save_messages(&pool, "msg-session-1", &msgs)
            .await
            .unwrap();
        let got = messages::get_messages_by_session(&pool, "msg-session-1")
            .await
            .unwrap();
        assert_eq!(got.len(), 2);

        let after = messages::get_messages_after_rowid(&pool, "msg-session-1", 1)
            .await
            .unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, "msg-2");
    }

    #[tokio::test]
    async fn test_provider_crud() {
        let pool = get_test_pool().await;

        let created = providers::create_provider(
            &pool,
            "p1",
            "OpenAI",
            "openai_compat",
            "https://api.openai.com",
        )
        .await
        .unwrap();
        assert_eq!(created.id, "p1");
        assert_eq!(created.name, "OpenAI");
        assert_eq!(created.provider_type, "openai_compat");

        let got = providers::get_provider(&pool, "p1").await.unwrap().unwrap();
        assert_eq!(got.id, "p1");

        let list = providers::list_providers(&pool).await.unwrap();
        assert_eq!(list.len(), 1);

        providers::update_provider(&pool, "p1", Some("Updated Name"), None, None)
            .await
            .unwrap();
        let updated = providers::get_provider(&pool, "p1").await.unwrap().unwrap();
        assert_eq!(updated.name, "Updated Name");

        providers::update_provider_status(&pool, "p1", "error", Some("timeout"))
            .await
            .unwrap();
        let updated = providers::get_provider(&pool, "p1").await.unwrap().unwrap();
        assert_eq!(updated.status, "error");
        assert_eq!(updated.error_message, Some("timeout".into()));

        let deleted = providers::delete_provider(&pool, "p1").await.unwrap();
        assert_eq!(deleted, 1);
    }

    #[tokio::test]
    async fn test_mcp_server_crud() {
        let pool = get_test_pool().await;

        mcp_servers::create_server(&pool, "s1", "kitty-tools", "stdio")
            .await
            .unwrap();

        let got = mcp_servers::get_server(&pool, "s1").await.unwrap().unwrap();
        assert_eq!(got.id, "s1");
        assert_eq!(got.name, "kitty-tools");
        assert_eq!(got.transport, "stdio");

        let list = mcp_servers::list_servers(&pool).await.unwrap();
        assert_eq!(list.len(), 1);

        mcp_servers::update_server(&pool, "s1", Some("kitty-docs-web"), None, None, Some(0))
            .await
            .unwrap();
        let updated = mcp_servers::get_server(&pool, "s1").await.unwrap().unwrap();
        assert_eq!(updated.name, "kitty-docs-web");
        assert_eq!(updated.enabled, 0);

        let deleted = mcp_servers::delete_server(&pool, "s1").await.unwrap();
        assert_eq!(deleted, 1);
    }

    #[tokio::test]
    async fn test_recipe_crud() {
        let pool = get_test_pool().await;

        recipes::create_recipe(
            &pool,
            "r1",
            "Code Review",
            "Review this code: {}",
            Some("Be thorough"),
            10,
        )
        .await
        .unwrap();

        let got = recipes::get_recipe(&pool, "r1").await.unwrap().unwrap();
        assert_eq!(got.id, "r1");
        assert_eq!(got.name, "Code Review");
        assert_eq!(got.max_steps, 10);

        let list = recipes::list_recipes(&pool).await.unwrap();
        assert_eq!(list.len(), 1);

        recipes::update_recipe(&pool, "r1", Some("Code Review v2"), None, None)
            .await
            .unwrap();
        let updated = recipes::get_recipe(&pool, "r1").await.unwrap().unwrap();
        assert_eq!(updated.name, "Code Review v2");

        let deleted = recipes::delete_recipe(&pool, "r1").await.unwrap();
        assert_eq!(deleted, 1);
    }

    #[tokio::test]
    async fn test_schedule_crud() {
        let pool = get_test_pool().await;
        recipes::create_recipe(
            &pool,
            "r1",
            "Code Review",
            "Review this code: {}",
            Some("Be thorough"),
            10,
        )
        .await
        .unwrap();

        schedules::create_schedule(&pool, "sch1", "Daily Review", "0 9 * * *", "r1", 1)
            .await
            .unwrap();

        let got = schedules::get_schedule(&pool, "sch1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.id, "sch1");
        assert_eq!(got.cron, "0 9 * * *");
        assert_eq!(got.enabled, 1);

        let list = schedules::list_schedules(&pool).await.unwrap();
        assert_eq!(list.len(), 1);

        schedules::update_schedule(&pool, "sch1", None, Some(0))
            .await
            .unwrap();
        let updated = schedules::get_schedule(&pool, "sch1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.enabled, 0);

        let deleted = schedules::delete_schedule(&pool, "sch1").await.unwrap();
        assert_eq!(deleted, 1);
    }

    #[tokio::test]
    async fn test_execution_crud() {
        let pool = get_test_pool().await;
        sessions::create_session(&pool, "sess-exec", "Exec Session")
            .await
            .unwrap();

        execution::insert_execution(&pool, "exec1", "sess-exec", "schedule", Some("sch1"))
            .await
            .unwrap();

        let execs = execution::get_executions_for_recipe(&pool, "sch1", 100)
            .await
            .unwrap();
        assert_eq!(execs.len(), 1);
        assert_eq!(execs[0].id, "exec1");
        assert_eq!(execs[0].status, "running");

        execution::update_execution_status(&pool, "exec1", "completed", Some("All done"), None)
            .await
            .unwrap();
        let execs = execution::get_executions_for_recipe(&pool, "sch1", 100)
            .await
            .unwrap();
        assert_eq!(execs[0].status, "completed");
        assert_eq!(execs[0].result_summary, Some("All done".into()));
    }

    #[tokio::test]
    async fn test_hitl_rule_crud() {
        let pool = get_test_pool().await;

        hitl_rules::upsert_rule(&pool, "browser.click", Some(".*"), "reject")
            .await
            .unwrap();

        let list = hitl_rules::list_rules(&pool).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].tool_name, "browser.click");
        assert_eq!(list[0].decision, "reject");

        let by_tool = hitl_rules::list_rules_by_tool(&pool, "browser.click")
            .await
            .unwrap();
        assert_eq!(by_tool.len(), 1);

        let deleted = hitl_rules::delete_rule(&pool, list[0].id).await.unwrap();
        assert_eq!(deleted, 1);
    }

    #[tokio::test]
    async fn hitl_rule_upsert_updates_in_place_instead_of_duplicating() {
        let pool = get_test_pool().await;

        // Same (tool_name, args_pattern) recorded twice — e.g. a user
        // clicking "always allow" for the same tool more than once — must
        // update the one row, not pile up a second.
        hitl_rules::upsert_rule(&pool, "shell.exec", None, "allow")
            .await
            .unwrap();
        hitl_rules::upsert_rule(&pool, "shell.exec", None, "always_allow")
            .await
            .unwrap();

        let rows = hitl_rules::list_rules_by_tool(&pool, "shell.exec")
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "expected one row, got {rows:?}");
        assert_eq!(rows[0].decision, "always_allow");

        // A different args_pattern for the same tool is a distinct rule.
        hitl_rules::upsert_rule(&pool, "shell.exec", Some("^rm "), "reject")
            .await
            .unwrap();
        let rows = hitl_rules::list_rules_by_tool(&pool, "shell.exec")
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[tokio::test]
    async fn test_timing_crud() {
        let pool = get_test_pool().await;
        sessions::create_session(&pool, "sess-tim", "Timings Session")
            .await
            .unwrap();

        let timing = timings::TimingRow {
            id: "tim1".into(),
            session_id: "sess-tim".into(),
            provider_id: Some("p1".into()),
            model: Some("gpt-4".into()),
            ttfb_ms: Some(120.5),
            ttft_ms: Some(250.0),
            generation_ms: Some(1500.0),
            total_tokens: Some(500),
            created_at: None,
        };
        timings::insert_timing(&pool, &timing).await.unwrap();

        let recent = timings::get_recent_timings(&pool, "sess-tim", 10)
            .await
            .unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].id, "tim1");
        assert!((recent[0].ttfb_ms.unwrap() - 120.5).abs() < f64::EPSILON);
    }


}
