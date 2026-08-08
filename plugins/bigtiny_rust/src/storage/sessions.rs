use chrono::DateTime;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{Connection, FromRow, SqlitePool};

use crate::error::StorageError;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct SessionRow {
    pub id: String,
    pub name: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub status: String,
    pub metadata: Option<String>,
    pub memory_slots: Option<String>,
    pub compacted_through_rowid: i64,
    pub compaction_state: Option<String>,
    pub compaction_started_at: Option<DateTime<Utc>>,
}

pub async fn get_session(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<Option<SessionRow>, StorageError> {
    let row = sqlx::query_as::<_, SessionRow>(
        r#"SELECT id, name, created_at, updated_at, status, metadata,
                  memory_slots, compacted_through_rowid, compaction_state, compaction_started_at
           FROM sessions WHERE id = ?"#,
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn list_sessions(pool: &SqlitePool) -> Result<Vec<SessionRow>, StorageError> {
    let rows = sqlx::query_as::<_, SessionRow>(
        r#"SELECT id, name, created_at, updated_at, status, metadata,
                  memory_slots, compacted_through_rowid, compaction_state, compaction_started_at
           FROM sessions ORDER BY updated_at DESC"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Paginated session list + total row count, matching the Python original
/// (`bigtiny/server/routes/chat.py::list_sessions`) — `GET /api/chat/`'s
/// `limit`/`offset` query params used to be parsed and then never applied to
/// the query at all, always returning every session with no `total`.
pub async fn list_sessions_page(
    pool: &SqlitePool,
    limit: i64,
    offset: i64,
) -> Result<(Vec<SessionRow>, i64), StorageError> {
    let rows = sqlx::query_as::<_, SessionRow>(
        r#"SELECT id, name, created_at, updated_at, status, metadata,
                  memory_slots, compacted_through_rowid, compaction_state, compaction_started_at
           FROM sessions ORDER BY updated_at DESC LIMIT ?1 OFFSET ?2"#,
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let total: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM sessions"#)
        .fetch_one(pool)
        .await?;
    Ok((rows, total))
}

pub async fn create_session(
    pool: &SqlitePool,
    id: &str,
    name: &str,
) -> Result<SessionRow, StorageError> {
    sqlx::query(r#"INSERT INTO sessions (id, name, status) VALUES (?, ?, 'active')"#)
        .bind(id)
        .bind(name)
        .execute(pool)
        .await?;
    get_session(pool, id)
        .await?
        .ok_or_else(|| StorageError::Generic(format!("Session {} not found after creation", id)))
}

pub async fn update_session(pool: &SqlitePool, session_id: &str) -> Result<(), StorageError> {
    sqlx::query(r#"UPDATE sessions SET updated_at = CURRENT_TIMESTAMP WHERE id = ?"#)
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_session(pool: &SqlitePool, session_id: &str) -> Result<u64, StorageError> {
    let result = sqlx::query(r#"DELETE FROM sessions WHERE id = ?"#)
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

pub async fn update_session_name(
    pool: &SqlitePool,
    session_id: &str,
    name: &str,
) -> Result<(), StorageError> {
    sqlx::query(r#"UPDATE sessions SET name = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?"#)
        .bind(name)
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn update_session_status(
    pool: &SqlitePool,
    session_id: &str,
    status: &str,
) -> Result<(), StorageError> {
    sqlx::query(r#"UPDATE sessions SET status = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?"#)
        .bind(status)
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn update_session_config(
    pool: &SqlitePool,
    session_id: &str,
    metadata: &str,
) -> Result<(), StorageError> {
    sqlx::query(r#"UPDATE sessions SET metadata = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?"#)
        .bind(metadata)
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Atomically read-modify-write `sessions.metadata` inside one transaction.
/// `SessionStats::record_usage` (fires after nearly every LLM call) and
/// `PATCH /api/chat/{id}/config` (mode/provider/cwd changes) both do a
/// read-then-write on this same column as two separate, unsynchronized pool
/// calls — nothing stopped them (or two concurrent calls to either) from
/// interleaving and one silently clobbering the other's just-written change
/// with its own stale read. `mutate` receives the current metadata
/// (`{}` if unset/unparseable) and returns the value to persist.
///
/// Uses an explicit `BEGIN IMMEDIATE` (acquired on a freshly-checked-out
/// connection) rather than the pool's default *deferred* `BEGIN`: a deferred
/// transaction takes its write lock only at the first UPDATE, so two
/// concurrent callers can both read the same snapshot and both later
/// overwrite each other — the very lost-update this function exists to
/// prevent. `BEGIN IMMEDIATE` grabs the write lock up front, serializing the
/// read-modify-write.
pub async fn update_metadata_with<F>(
    pool: &SqlitePool,
    session_id: &str,
    mutate: F,
) -> Result<(), StorageError>
where
    F: FnOnce(serde_json::Value) -> serde_json::Value,
{
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin_with("BEGIN IMMEDIATE").await?;

    let row: Option<(Option<String>,)> =
        sqlx::query_as(r#"SELECT metadata FROM sessions WHERE id = ?"#)
            .bind(session_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((metadata_str,)) = row else {
        return Err(StorageError::Generic(format!(
            "Session {} not found",
            session_id
        )));
    };

    let current: serde_json::Value = metadata_str
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    let next = mutate(current);
    let next_json = serde_json::to_string(&next)
        .map_err(|e| StorageError::Generic(format!("Failed to serialize metadata: {}", e)))?;

    sqlx::query(r#"UPDATE sessions SET metadata = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?"#)
        .bind(&next_json)
        .bind(session_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(())
}

pub async fn get_session_metadata(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<Option<String>, StorageError> {
    let metadata: Option<(String,)> =
        sqlx::query_as(r#"SELECT metadata FROM sessions WHERE id = ?"#)
            .bind(session_id)
            .fetch_optional(pool)
            .await?;
    Ok(metadata.map(|t| t.0))
}

/// Atomically claim the compaction lock for a session via compare-and-swap:
/// succeeds if the lock isn't held, or if it's held but stale (older than
/// `stale_after`, reclaiming a lock left behind by a crashed/killed pass).
/// Returns `true` iff this call acquired the lock. Always pair with
/// `release_compaction_lock` on every exit path — compaction fires
/// fire-and-forget after every turn, so without this lock, overlapping
/// triggers for the same session can race on `compacted_through_rowid`.
pub async fn try_acquire_compaction_lock(
    pool: &SqlitePool,
    session_id: &str,
    stale_after: chrono::Duration,
) -> Result<bool, StorageError> {
    let cutoff = (chrono::Utc::now() - stale_after)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let result = sqlx::query(
        r#"UPDATE sessions SET compaction_state = 'running', compaction_started_at = CURRENT_TIMESTAMP
           WHERE id = ?1 AND (
               compaction_state IS NULL OR compaction_state != 'running'
               OR compaction_started_at IS NULL OR compaction_started_at < ?2
           )"#
    )
    .bind(session_id)
    .bind(cutoff)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Release the compaction lock, whether or not the pass succeeded.
pub async fn release_compaction_lock(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<(), StorageError> {
    sqlx::query(r#"UPDATE sessions SET compaction_state = 'idle' WHERE id = ?"#)
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn update_compaction_state(
    pool: &SqlitePool,
    session_id: &str,
    memory_slots: &str,
    compacted_through_rowid: i64,
) -> Result<(), StorageError> {
    sqlx::query(
        r#"UPDATE sessions SET memory_slots = ?, compacted_through_rowid = ?, compaction_state = 'idle', updated_at = CURRENT_TIMESTAMP WHERE id = ?"#
    )
    .bind(memory_slots)
    .bind(compacted_through_rowid)
    .bind(session_id)
    .execute(pool)
    .await?;
    Ok(())
}
