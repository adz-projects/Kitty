use chrono::DateTime;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{Connection, FromRow, SqlitePool};

use crate::error::StorageError;

pub type HITLRule = HITLRuleRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct HITLRuleRow {
    pub id: i64,
    pub tool_name: String,
    pub args_pattern: Option<String>,
    pub decision: String,
    pub created_at: Option<DateTime<Utc>>,
}

pub async fn list_rules(pool: &SqlitePool) -> Result<Vec<HITLRuleRow>, StorageError> {
    let rows = sqlx::query_as::<_, HITLRuleRow>(
        r#"SELECT id, tool_name, args_pattern, decision, created_at FROM hitl_rules ORDER BY id ASC"#
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn list_rules_by_tool(
    pool: &SqlitePool,
    tool_name: &str,
) -> Result<Vec<HITLRuleRow>, StorageError> {
    let rows = sqlx::query_as::<_, HITLRuleRow>(
        r#"SELECT id, tool_name, args_pattern, decision, created_at FROM hitl_rules WHERE tool_name = ?"#
    )
    .bind(tool_name)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// `hitl_rules` has no UNIQUE constraint `ON CONFLICT` could target (only
/// the autoincrement `id`), and even adding one on `(tool_name,
/// args_pattern)` wouldn't make a SQL-level upsert work for the common
/// `args_pattern IS NULL` case anyway — SQL treats every `NULL` as distinct
/// from every other `NULL` for uniqueness purposes, so two "always allow
/// this tool, any args" rows would never conflict. Do the check-then-act
/// explicitly instead (`IS` rather than `=` so `NULL` compares equal to
/// `NULL` the way this needs), so recording the same decision repeatedly
/// updates one row instead of piling up duplicates that all still happen to
/// resolve to the same effective policy.
pub async fn upsert_rule(
    pool: &SqlitePool,
    tool_name: &str,
    args_pattern: Option<&str>,
    decision: &str,
) -> Result<(), StorageError> {
    // `BEGIN IMMEDIATE`, not the pool's default deferred `BEGIN` (same
    // reasoning as `sessions::update_metadata_with`): the check-then-act
    // below reads before it writes, and under a deferred transaction two
    // concurrent same-key upserts can each read "no row" on their own
    // snapshot and then both try to INSERT — the loser dies with
    // `SQLITE_BUSY_SNAPSHOT`, surfacing as a spurious 500 on an approval
    // click. Grabbing the write lock up front serializes the pair instead.
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin_with("BEGIN IMMEDIATE").await?;

    let existing: Option<i64> = sqlx::query_scalar(
        r#"SELECT id FROM hitl_rules WHERE tool_name = ?1 AND args_pattern IS ?2"#,
    )
    .bind(tool_name)
    .bind(args_pattern)
    .fetch_optional(&mut *tx)
    .await?;

    match existing {
        Some(id) => {
            sqlx::query(r#"UPDATE hitl_rules SET decision = ? WHERE id = ?"#)
                .bind(decision)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        None => {
            sqlx::query(
                r#"INSERT INTO hitl_rules (tool_name, args_pattern, decision) VALUES (?, ?, ?)"#,
            )
            .bind(tool_name)
            .bind(args_pattern)
            .bind(decision)
            .execute(&mut *tx)
            .await?;
        }
    }

    tx.commit().await?;
    Ok(())
}

pub async fn delete_rule(pool: &SqlitePool, rule_id: i64) -> Result<u64, StorageError> {
    let result = sqlx::query(r#"DELETE FROM hitl_rules WHERE id = ?"#)
        .bind(rule_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}
