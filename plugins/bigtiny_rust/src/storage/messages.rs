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
    // Scope the dedupe lookup to *this batch's* ids (`WHERE ... AND id IN
    // (...)`), not the whole session. The previous version re-read every
    // message id in the session on every save — O(n) per call, and it runs
    // after nearly every agent-loop step, so a session's cost grew
    // quadratically over a long turn. Behavior is identical: a message whose
    // id already exists (in this batch or in the session) is skipped.
    let batch_ids: Vec<&str> = messages.iter().map(|m| m.id.as_str()).collect();
    let existing_ids: HashSet<String> = if batch_ids.is_empty() {
        HashSet::new()
    } else {
        let placeholders: Vec<&str> = batch_ids.iter().map(|_| "?").collect();
        let query = format!(
            "SELECT id FROM messages WHERE session_id = ? AND id IN ({})",
            placeholders.join(",")
        );
        let mut q = sqlx::query_scalar::<_, String>(&query).bind(session_id);
        for id in &batch_ids {
            q = q.bind(id);
        }
        q.fetch_all(pool).await?.into_iter().collect()
    };

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

/// Fetch only the most recent `limit` messages in chronological (insert)
/// order, by pushing `LIMIT` into SQL (`ORDER BY rowid DESC LIMIT ?` then
/// reversed) — the previous `GET /api/chat/{id}/history` used to fetch the
/// *entire* session and truncate client-side, which re-read every message
/// even for tiny `limit` values. A `limit <= 0` is treated as "no limit"
/// (SQLite's own `LIMIT` semantics), matching the old truncate behavior.
pub async fn get_last_messages_by_session(
    pool: &SqlitePool,
    session_id: &str,
    limit: i64,
) -> Result<Vec<MessageRow>, StorageError> {
    let mut rows = sqlx::query_as::<_, MessageRow>(
        r#"SELECT rowid, id, session_id, role, content, tool_calls, tool_call_id, token_count, content_format, created_at
           FROM messages WHERE session_id = ? ORDER BY rowid DESC LIMIT ?"#
    )
    .bind(session_id)
    .bind(limit.max(0))
    .fetch_all(pool)
    .await?;
    rows.reverse();
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn msg(id: &str, session_id: &str, role: &str) -> MessageRow {
        MessageRow {
            rowid: 0,
            id: id.into(),
            session_id: session_id.into(),
            role: role.into(),
            content: None,
            tool_calls: None,
            tool_call_id: None,
            token_count: None,
            content_format: None,
            created_at: None,
        }
    }

    /// Regression (WS6-3): after scoping the dedupe lookup to the batch's ids
    /// (`WHERE session_id = ? AND id IN (...)`), a message already saved must
    /// still never be inserted twice, and new ones must still be inserted.
    #[tokio::test]
    async fn scoped_dedupe_still_skips_already_saved_ids() {
        let pool = test_pool().await;
        // `messages.session_id` is FK-enforced against `sessions.id`.
        sqlx::query("INSERT INTO sessions (id, status) VALUES ('s1', 'active')")
            .execute(&pool)
            .await
            .unwrap();

        save_messages(
            &pool,
            "s1",
            &[msg("a", "s1", "user"), msg("b", "s1", "assistant")],
        )
        .await
        .unwrap();

        // Second batch: `a` is a duplicate (must be skipped), `c` is new, and
        // `sys` (role == system) is never persisted regardless.
        save_messages(
            &pool,
            "s1",
            &[msg("a", "s1", "user"), msg("c", "s1", "user"), msg("sys", "s1", "system")],
        )
        .await
        .unwrap();

        let rows = sqlx::query_as::<_, MessageRow>(
            "SELECT rowid, id, session_id, role, content, tool_calls, tool_call_id, token_count, content_format, created_at \
             FROM messages WHERE session_id = 's1' ORDER BY rowid ASC",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }
}
