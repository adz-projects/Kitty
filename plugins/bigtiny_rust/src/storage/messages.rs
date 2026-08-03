use chrono::DateTime;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use std::collections::HashSet;

use crate::error::StorageError;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct MessageRow {
    pub rowid: i64,
    pub id: String,
    pub session_id: String,
    pub role: String,
    pub content: Option<String>,
    pub tool_calls: Option<String>,
    pub tool_call_id: Option<String>,
    pub token_count: Option<i32>,
    pub content_format: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
}

pub async fn save_messages(
    pool: &SqlitePool,
    session_id: &str,
    messages: &[MessageRow],
) -> Result<(), StorageError> {
    // A HashSet, not a Vec — this is checked once per message below, and a
    // linear `Vec::contains` scan per message made the whole function O(n^2)
    // for a session with n prior messages (called after nearly every step).
    let existing_ids: HashSet<String> =
        sqlx::query_scalar::<_, String>(r#"SELECT id FROM messages WHERE session_id = ?"#)
            .bind(session_id)
            .fetch_all(pool)
            .await?
            .into_iter()
            .collect();

    let mut tx = pool.begin().await?;
    for msg in messages {
        if !existing_ids.contains(&msg.id) && msg.role != "system" {
            sqlx::query(
                r#"INSERT INTO messages (id, session_id, role, content, tool_calls, tool_call_id, token_count, content_format)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?)"#
            )
            .bind(&msg.id)
            .bind(&msg.session_id)
            .bind(&msg.role)
            .bind(&msg.content)
            .bind(&msg.tool_calls)
            .bind(&msg.tool_call_id)
            .bind(msg.token_count.unwrap_or(0))
            .bind(&msg.content_format)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

pub async fn get_messages_by_session(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<Vec<MessageRow>, StorageError> {
    let rows = sqlx::query_as::<_, MessageRow>(
        r#"SELECT rowid, id, session_id, role, content, tool_calls, tool_call_id, token_count, content_format, created_at
           FROM messages WHERE session_id = ? ORDER BY rowid ASC"#
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get_messages_after_rowid(
    pool: &SqlitePool,
    session_id: &str,
    after_rowid: i64,
) -> Result<Vec<MessageRow>, StorageError> {
    let rows = sqlx::query_as::<_, MessageRow>(
        r#"SELECT rowid, id, session_id, role, content, tool_calls, tool_call_id, token_count, content_format, created_at
           FROM messages WHERE session_id = ? AND rowid > ? ORDER BY rowid ASC"#
    )
    .bind(session_id)
    .bind(after_rowid)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get_first_user_message(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<Option<MessageRow>, StorageError> {
    let row = sqlx::query_as::<_, MessageRow>(
        r#"SELECT rowid, id, session_id, role, content, tool_calls, tool_call_id, token_count, content_format, created_at
           FROM messages WHERE session_id = ? AND role = 'user' ORDER BY rowid ASC LIMIT 1"#
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}
