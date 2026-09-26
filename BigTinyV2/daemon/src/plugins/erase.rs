//! "Erase all" for a memory plugin's database.
//!
//! Clears every row in place rather than deleting the database file. A file
//! delete fails on Windows while anything still holds the database open -- and
//! an in-flight turn, a background sweep or an MCP server connection all may --
//! whereas clearing rows works through the same pool everything else is using
//! and leaves the schema and migrations intact.

use sqlx::{Connection, SqlitePool};

/// Delete every row from every ordinary table in `pool`'s database, in one
/// transaction. Returns how many tables were cleared.
///
/// Left alone:
/// * SQLite's own tables and `_sqlx_migrations` (the schema history);
/// * FTS5 tables with external content (`content='...'`) and every virtual
///   table's shadow tables -- the base table's delete triggers keep an
///   external-content index in step, and touching shadow tables directly
///   corrupts the index.
///
/// Other virtual tables (a `vec0` vector index, a self-contained FTS index) are
/// cleared with a plain `DELETE`, which both support.
///
/// Foreign keys are deferred to commit, so tables can be cleared in any order.
pub async fn erase_all_rows(pool: &SqlitePool) -> Result<usize, sqlx::Error> {
    let tables: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT name, sql FROM sqlite_master WHERE type = 'table'")
            .fetch_all(pool)
            .await?;
    let is_virtual = |sql: &Option<String>| {
        sql.as_deref()
            .is_some_and(|s| s.trim_start().to_ascii_uppercase().starts_with("CREATE VIRTUAL TABLE"))
    };
    let virtual_names: Vec<&str> = tables
        .iter()
        .filter(|(_, sql)| is_virtual(sql))
        .map(|(name, _)| name.as_str())
        .collect();

    let targets: Vec<&str> = tables
        .iter()
        .filter(|(name, sql)| {
            if name.starts_with("sqlite_") || name == "_sqlx_migrations" {
                return false;
            }
            // A shadow table of some virtual table (`beliefs_fts_data`, ...).
            if virtual_names
                .iter()
                .any(|v| name != v && name.starts_with(&format!("{v}_")))
            {
                return false;
            }
            // External-content FTS is maintained by triggers on its base table.
            if is_virtual(sql)
                && sql
                    .as_deref()
                    .is_some_and(|s| s.to_ascii_lowercase().contains("content="))
            {
                return false;
            }
            true
        })
        .map(|(name, _)| name.as_str())
        .collect();

    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    sqlx::query("PRAGMA defer_foreign_keys = ON")
        .execute(&mut *tx)
        .await?;
    for table in &targets {
        let quoted = table.replace('"', "\"\"");
        sqlx::query(&format!("DELETE FROM \"{quoted}\""))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(targets.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape both engines actually have: a base table with an
    /// external-content FTS index kept in step by triggers, a child table with
    /// a foreign key, and the migrations table that must survive.
    #[tokio::test]
    async fn everything_but_the_schema_is_cleared() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        for stmt in [
            "PRAGMA foreign_keys = ON",
            "CREATE TABLE _sqlx_migrations (version INTEGER)",
            "INSERT INTO _sqlx_migrations VALUES (1)",
            "CREATE TABLE beliefs (id TEXT PRIMARY KEY, text TEXT)",
            "CREATE TABLE evidence (id INTEGER PRIMARY KEY, belief_id TEXT REFERENCES beliefs(id))",
            "CREATE VIRTUAL TABLE beliefs_fts USING fts5(text, content='beliefs', content_rowid='rowid')",
            "CREATE TRIGGER beliefs_ad AFTER DELETE ON beliefs BEGIN \
               INSERT INTO beliefs_fts(beliefs_fts, rowid, text) VALUES ('delete', OLD.rowid, OLD.text); END",
            "CREATE TRIGGER beliefs_ai AFTER INSERT ON beliefs BEGIN \
               INSERT INTO beliefs_fts(rowid, text) VALUES (NEW.rowid, NEW.text); END",
            "INSERT INTO beliefs VALUES ('b1', 'likes tea')",
            "INSERT INTO evidence (belief_id) VALUES ('b1')",
        ] {
            sqlx::query(stmt).execute(&pool).await.unwrap();
        }

        erase_all_rows(&pool).await.unwrap();

        let count = |t: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*) FROM {t}"))
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(count("beliefs").await, 0);
        assert_eq!(count("evidence").await, 0);
        assert_eq!(count("_sqlx_migrations").await, 1, "schema history survives");
        let hits: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM beliefs_fts WHERE beliefs_fts MATCH 'tea'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(hits, 0, "the search index was kept in step");
    }
}
