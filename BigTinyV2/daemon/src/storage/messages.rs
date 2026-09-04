use chrono::DateTime;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use std::collections::HashSet;
use std::time::Duration;

use crate::error::StorageError;

/// Token/count aggregates for a session's `messages` table, computed in SQL
/// (no full-table materialization — the old `get_stats` caller re-read *every*
/// row on each poll of the stats route, which grew linearly with session
/// length).
#[derive(Debug, Clone, FromRow)]
pub struct MessageAggregates {
    pub count: i64,
    pub user_system_tokens: i64,
    pub assistant_tokens: i64,
    pub all_tokens: i64,
}

pub async fn session_message_aggregates(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<MessageAggregates, StorageError> {
    let row = sqlx::query_as::<_, MessageAggregates>(
        r#"SELECT
               COUNT(*) AS count,
               COALESCE(SUM(CASE WHEN role IN ('user', 'system') THEN token_count ELSE 0 END), 0)
                 AS user_system_tokens,
               COALESCE(SUM(CASE WHEN role = 'assistant' THEN token_count ELSE 0 END), 0)
                 AS assistant_tokens,
               COALESCE(SUM(token_count), 0) AS all_tokens
           FROM messages WHERE session_id = ?"#,
    )
    .bind(session_id)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

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

/// SQLITE_BUSY (code 5) surfaces as `database is locked` / `database table
/// is locked` — a concurrent writer (e.g. a compaction pass or another
/// turn) holds the write lock. Retrying shortly is safe: every insert is
/// inside a transaction, so a busy failure committed nothing.
fn is_busy_error(e: &StorageError) -> bool {
    match e {
        StorageError::Sqlx(sqlx::Error::Database(db)) => {
            db.code().as_deref() == Some("5")
                || db.message().to_lowercase().contains("database is locked")
        }
        _ => false,
    }
}

pub async fn save_messages(
    pool: &SqlitePool,
    session_id: &str,
    messages: &[MessageRow],
) -> Result<(), StorageError> {
    // A transient SQLITE_BUSY used to abort the whole save; the caller
    // warned and moved on, and a later save re-inserted the same content
    // under fresh UUIDs — duplicate transcript rows. Retry the batch a few
    // times with a short linear backoff before giving up.
    const BUSY_SAVE_RETRIES: u32 = 3;
    const BUSY_SAVE_RETRY_DELAY_MS: u64 = 50;
    let mut attempt = 0u32;
    loop {
        match save_messages_once(pool, session_id, messages).await {
            Ok(()) => return Ok(()),
            Err(e) if is_busy_error(&e) && attempt < BUSY_SAVE_RETRIES => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(
                    BUSY_SAVE_RETRY_DELAY_MS * attempt as u64,
                ))
                .await;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn save_messages_once(
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
    // Track ids inserted DURING this batch so two messages in the SAME save
    // sharing an id don't both pass the DB check and then abort the whole
    // transaction with a UNIQUE violation — the doc-comment promises batch-
    // level dedupe, but the SQL lookup above only covers already-persisted
    // rows, not in-flight duplicates within this batch.
    let mut inserted_this_batch: HashSet<String> = HashSet::new();
    for msg in messages {
        if existing_ids.contains(&msg.id) || inserted_this_batch.contains(&msg.id) {
            continue;
        }
        if msg.role != "system" {
            sqlx::query(
                r#"INSERT INTO messages (id, session_id, role, content, tool_calls, tool_call_id, token_count, content_format)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?)"#
            )
            .bind(&msg.id)
            // Bind the *function parameter*, not `msg.session_id`: the dedupe
            // lookup above is scoped to `session_id`, so a row carrying a
            // different session_id would both escape dedupe and land in the
            // wrong session. The parameter is where the caller said this
            // batch belongs.
            .bind(session_id)
            .bind(&msg.role)
            .bind(&msg.content)
            .bind(&msg.tool_calls)
            .bind(&msg.tool_call_id)
            .bind(msg.token_count.unwrap_or(0))
            .bind(&msg.content_format)
            .execute(&mut *tx)
            .await?;
            inserted_this_batch.insert(msg.id.clone());
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
/// even for tiny `limit` values. A `limit <= 0` is treated as "no limit" —
/// pass the value through unchanged, since SQLite treats a NEGATIVE limit as
/// "no limit"; the old `.max(0)` turned `<= 0` into `LIMIT 0`, which returns
/// ZERO rows (the exact opposite of the documented contract).
pub async fn get_last_messages_by_session(
    pool: &SqlitePool,
    session_id: &str,
    limit: i64,
) -> Result<Vec<MessageRow>, StorageError> {
    // `limit <= 0` (including exactly 0, e.g. `?limit=0`) means "no limit" per
    // the doc above — SQLite itself only treats a NEGATIVE LIMIT that way,
    // so 0 must be normalized to -1 or it silently returns zero rows.
    let effective_limit = if limit <= 0 { -1 } else { limit };
    let mut rows = sqlx::query_as::<_, MessageRow>(
        r#"SELECT rowid, id, session_id, role, content, tool_calls, tool_call_id, token_count, content_format, created_at
           FROM messages WHERE session_id = ? ORDER BY rowid DESC LIMIT ?"#
    )
    .bind(session_id)
    .bind(effective_limit)
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

/// Top FTS5 matches for `fts_query` against a session's *compacted* history
/// (`rowid <= compacted_through`), restricted to `user`/`assistant` dialogue
/// (raw tool output is not a useful recall target). Returns up to `limit`
/// `(rowid, bm25_score)` pairs, best-first.
///
/// The BM25 relevance gate is deliberately **not** applied here: scores are
/// returned raw so the caller can `tracing::debug!` every candidate (the
/// empirical dataset the `bm25_threshold` tuning is meant to collect) and
/// reject below-bar matches in code. FTS5 BM25 scores are negative, and MORE
/// NEGATIVE = MORE relevant (best matches sort first under `ORDER BY rank
/// ASC`) — the gate keeps `score <= t`, never the reverse.
pub async fn best_compacted_matches(
    pool: &SqlitePool,
    session_id: &str,
    fts_query: &str,
    compacted_through: i64,
    limit: i64,
) -> Result<Vec<(i64, f64)>, StorageError> {
    let rows = sqlx::query_as::<_, (i64, f64)>(
        r#"SELECT m.rowid, bm25(messages_fts) AS rank
           FROM messages_fts f
           JOIN messages m ON f.rowid = m.rowid
           WHERE messages_fts MATCH ?1 AND m.session_id = ?2 AND m.rowid <= ?3
             AND m.role IN ('user', 'assistant')
           ORDER BY rank ASC LIMIT ?4"#,
    )
    .bind(fts_query)
    .bind(session_id)
    .bind(compacted_through)
    .bind(limit.max(0))
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The `user` message that opens the exchange containing `rowid` (the `user`
/// message at or before it). Used to assemble a clean recall exchange.
pub async fn exchange_anchor(
    pool: &SqlitePool,
    session_id: &str,
    rowid: i64,
) -> Result<Option<MessageRow>, StorageError> {
    let row = sqlx::query_as::<_, MessageRow>(
        r#"SELECT rowid, id, session_id, role, content, tool_calls, tool_call_id, token_count, content_format, created_at
           FROM messages WHERE session_id = ? AND role = 'user' AND rowid <= ?
           ORDER BY rowid DESC LIMIT 1"#,
    )
    .bind(session_id)
    .bind(rowid)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// The first `user` message with `rowid > start_rowid`, if any — the exclusive
/// upper bound of the exchange starting at `start_rowid`.
pub async fn next_user_after(
    pool: &SqlitePool,
    session_id: &str,
    start_rowid: i64,
) -> Result<Option<MessageRow>, StorageError> {
    let row = sqlx::query_as::<_, MessageRow>(
        r#"SELECT rowid, id, session_id, role, content, tool_calls, tool_call_id, token_count, content_format, created_at
           FROM messages WHERE session_id = ? AND role = 'user' AND rowid > ?
           ORDER BY rowid ASC LIMIT 1"#,
    )
    .bind(session_id)
    .bind(start_rowid)
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
            &[
                msg("a", "s1", "user"),
                msg("c", "s1", "user"),
                msg("sys", "s1", "system"),
            ],
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

    /// Regression (88bugs #82): `limit == 0` must mean "no limit" per this
    /// function's own doc comment, not "zero rows" — a client hitting
    /// `GET /api/chat/{id}/history?limit=0` previously got an empty result
    /// even though `limit <= 0` was documented as unbounded.
    #[tokio::test]
    async fn zero_limit_means_no_limit() {
        let pool = test_pool().await;
        sqlx::query("INSERT INTO sessions (id, status) VALUES ('s1', 'active')")
            .execute(&pool)
            .await
            .unwrap();
        save_messages(
            &pool,
            "s1",
            &[
                msg("a", "s1", "user"),
                msg("b", "s1", "assistant"),
                msg("c", "s1", "user"),
            ],
        )
        .await
        .unwrap();

        let rows = get_last_messages_by_session(&pool, "s1", 0).await.unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);

        // A genuinely bounded limit still works.
        let rows = get_last_messages_by_session(&pool, "s1", 2).await.unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "c"]);
    }

    /// Regression: an injected memory block (`role: "system"` headed by one of
    /// the internal markers) must never be persisted by `save_messages`, even
    /// when it rides along in the same batch as a real user message. The
    /// frontend's `stripInternalMarkers` is a defense-in-depth net; the real
    /// guarantee lives here — the backend drops every `system`-role message
    /// before it reaches the `messages` table.
    #[tokio::test]
    async fn injected_memory_system_blocks_are_never_persisted() {
        let pool = test_pool().await;
        sqlx::query("INSERT INTO sessions (id, status) VALUES ('s1', 'active')")
            .execute(&pool)
            .await
            .unwrap();

        let mut recall = msg("m1", "s1", "system");
        recall.content = Some(
            "[Earlier context from this session]\nuser: Keep the API key out of the summary.\n\
             assistant: Understood.\n\n(Use the above only as background context...)\n"
                .to_string(),
        );
        let mut memory = msg("m2", "s1", "system");
        memory.content = Some(
            "[CONSOLIDATED PROJECT MEMORY]\nkey: invoice pipeline is rate-limited\n".to_string(),
        );
        let user = msg("u1", "s1", "user");

        save_messages(&pool, "s1", &[recall, memory, user])
            .await
            .unwrap();

        let rows = sqlx::query_as::<_, MessageRow>(
            "SELECT rowid, id, session_id, role, content, tool_calls, tool_call_id, token_count, content_format, created_at \
             FROM messages WHERE session_id = 's1' ORDER BY rowid ASC",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        let persisted: Vec<(String, String)> = rows
            .iter()
            .map(|r| (r.id.clone(), r.role.clone()))
            .collect();
        assert_eq!(persisted, vec![("u1".to_string(), "user".to_string())]);
    }
}
