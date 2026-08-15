use chrono::DateTime;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};

use crate::error::StorageError;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ExecutionRow {
    pub id: String,
    pub session_id: String,
    pub trigger_type: String,
    pub trigger_id: Option<String>,
    pub status: String,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub result_summary: Option<String>,
    pub error_message: Option<String>,
}

pub async fn insert_execution(
    pool: &SqlitePool,
    id: &str,
    session_id: &str,
    trigger_type: &str,
    trigger_id: Option<&str>,
) -> Result<(), StorageError> {
    sqlx::query(
        r#"INSERT INTO execution_history (id, session_id, trigger_type, trigger_id, status)
           VALUES (?, ?, ?, ?, 'running')"#,
    )
    .bind(id)
    .bind(session_id)
    .bind(trigger_type)
    .bind(trigger_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_execution_status(
    pool: &SqlitePool,
    execution_id: &str,
    status: &str,
    result_summary: Option<&str>,
    error_message: Option<&str>,
) -> Result<(), StorageError> {
    sqlx::query(
        r#"UPDATE execution_history SET
           status = ?,
           result_summary = ?,
           error_message = ?,
           completed_at = CASE WHEN ? != 'running' THEN CURRENT_TIMESTAMP ELSE completed_at END
           WHERE id = ?"#,
    )
    .bind(status)
    .bind(result_summary)
    .bind(error_message)
    .bind(status)
    .bind(execution_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_executions_for_recipe(
    pool: &SqlitePool,
    recipe_id: &str,
    limit: i64,
) -> Result<Vec<ExecutionRow>, StorageError> {
    // `LIMIT` pushed into SQL (and `trigger_id` indexed by migration 014) —
    // `execution_history` only ever grows, so an unbounded
    // `WHERE trigger_id = ?` was a full table scan materializing every
    // historical run on each call.
    let rows = sqlx::query_as::<_, ExecutionRow>(
        r#"SELECT id, session_id, trigger_type, trigger_id, status, started_at, completed_at, result_summary, error_message
           FROM execution_history WHERE trigger_id = ? ORDER BY started_at DESC LIMIT ?"#
    )
    .bind(recipe_id)
    .bind(limit.max(0))
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
