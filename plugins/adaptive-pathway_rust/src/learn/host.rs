//! The coupling seam between the engine and the *host* `bigtiny.db`. All
//! cross-db reads live here and nowhere else. If this grows past two queries,
//! the boundary is wrong.

use sqlx::SqlitePool;

use crate::error::{PathwayError, Result};

/// Read user/assistant messages in `(after_rowid, <= through_rowid]` from the
/// host db, dropping `role='system'` rows, and truncate to `max_chars`
/// keeping the newest. Returns the joined text (empty when nothing learnable).
pub async fn read_unlearned_chunk(
    host_pool: &SqlitePool,
    session_id: &str,
    after_rowid: i64,
    through_rowid: i64,
) -> Result<String> {
    // Query 1 of the two-host-read coupling seam.
    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT role, content, rowid FROM messages \
         WHERE session_id = ? AND rowid > ? AND rowid <= ? AND role != 'system' \
         ORDER BY rowid ASC",
    )
    .bind(session_id)
    .bind(after_rowid)
    .bind(through_rowid)
    .fetch_all(host_pool)
    .await
    .map_err(|e| PathwayError::Host(format!("bigtiny.db read: {e}")))?;

    // Join newest-first then truncate to max_chars, but keep chronological
    // order for the prompt.
    const MAX_CHARS: usize = 12000;
    let total: usize = rows.iter().map(|(_, c, _)| c.len()).sum();
    if total > MAX_CHARS {
        // keep newest: walk from the back accumulating until MAX_CHARS
        let mut kept: Vec<(String, String, i64)> = Vec::new();
        let mut acc = 0;
        for (role, content, rowid) in rows.iter().rev() {
            acc += content.len();
            kept.push((role.clone(), content.clone(), *rowid));
            if acc >= MAX_CHARS {
                break;
            }
        }
        kept.reverse();
        Ok(kept.iter().map(|(r, c, _)| format!("{r}: {c}")).collect::<Vec<_>>().join("\n"))
    } else {
        Ok(rows
            .iter()
            .map(|(r, c, _)| format!("{r}: {c}"))
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

/// List session ids whose conversation is idle (or stale-active) and ready to
/// consolidate. Query 2 of the coupling seam.
pub async fn idle_session_ids(
    host_pool: &SqlitePool,
    idle_cutoff: chrono::DateTime<chrono::Utc>,
    active_cutoff: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT id FROM sessions WHERE \
         (status = 'idle' AND updated_at < ?) OR \
         (status = 'active' AND updated_at < ?)",
    )
    .bind(idle_cutoff)
    .bind(active_cutoff)
    .fetch_all(host_pool)
    .await
    .map_err(|e| PathwayError::Host(format!("bigtiny.db sessions read: {e}")))?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

/// The rowid of the newest message in a session from the host db (or 0).
pub async fn session_max_rowid(host_pool: &SqlitePool, session_id: &str) -> Result<i64> {
    let max: Option<i64> =
        sqlx::query_scalar("SELECT MAX(rowid) FROM messages WHERE session_id = ?")
            .bind(session_id)
            .fetch_one(host_pool)
            .await
            .map_err(|e| PathwayError::Host(format!("bigtiny.db read: {e}")))?;
    Ok(max.unwrap_or(0))
}
