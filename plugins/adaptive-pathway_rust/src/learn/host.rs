//! The coupling seam between the engine and the *host* `bigtiny.db`. All
//! cross-db reads live here and nowhere else. If this grows past two queries,
//! the boundary is wrong.

use sqlx::SqlitePool;

use crate::error::{PathwayError, Result};

/// Head/tail bytes kept when eliding a `role='tool'` message's content,
/// mirroring the daemon's own `tool_mask_head`/`tool_mask_tail` defaults
/// (`plugins/bigtiny_rust/src/config.rs`) -- this crate can't read that
/// config (the host-read coupling seam is deliberately just two queries,
/// not a config dependency), so these are fixed rather than threaded
/// through.
const TOOL_MASK_HEAD: usize = 400;
const TOOL_MASK_TAIL: usize = 400;

/// Elide the middle of a long `role='tool'` message so one large tool
/// result can't dominate (or blow past) the chunk's char budget. Without
/// this, a single large tool output (a full file read, a big shell
/// command's stdout) could consume the entire `MAX_CHARS` window on its
/// own, leaving nothing for the actual conversational content the
/// extractor is supposed to learn from. Rounds to char boundaries so a
/// multi-byte UTF-8 character straddling the cut point never panics.
fn mask_tool_content(role: &str, content: &str) -> String {
    if role != "tool" || content.len() <= TOOL_MASK_HEAD + TOOL_MASK_TAIL {
        return content.to_string();
    }
    let head_idx = floor_char_boundary(content, TOOL_MASK_HEAD);
    let tail_idx = ceil_char_boundary(content, content.len() - TOOL_MASK_TAIL);
    if head_idx >= tail_idx {
        return content.to_string();
    }
    format!(
        "{}\n[...{} bytes elided...]\n{}",
        &content[..head_idx],
        tail_idx - head_idx,
        &content[tail_idx..]
    )
}

fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Read user/assistant/tool messages in `(after_rowid, <= through_rowid]`
/// from the host db, dropping `role='system'` rows, eliding large tool
/// results, and truncating to `max_chars` keeping the newest. Returns the
/// joined text (empty when nothing learnable).
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

    let rows: Vec<(String, String, i64)> = rows
        .into_iter()
        .map(|(role, content, rowid)| {
            let masked = mask_tool_content(&role, &content);
            (role, masked, rowid)
        })
        .collect();

    // Join newest-first then truncate to max_chars, but keep chronological
    // order for the prompt. Keeping a whole message once its addition
    // crosses the budget (rather than cutting off before it) is deliberate
    // -- never truncate mid-message.
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
///
/// Deliberately compares `updated_at` against SQLite's own `datetime('now',
/// ...)` rather than binding a `chrono::DateTime` cutoff computed in Rust.
/// `bigtiny.db` writes `updated_at` via bare `CURRENT_TIMESTAMP`
/// (`storage/sessions.rs`), which SQLite renders as naive
/// `'YYYY-MM-DD HH:MM:SS'` text with a space separator; sqlx encodes a bound
/// `chrono::DateTime<Utc>` as RFC3339 (`'YYYY-MM-DDTHH:MM:SS+00:00'`, a `T`
/// separator). SQLite compares TEXT columns byte-for-byte, and `' '` (0x20)
/// sorts before `'T'` (0x54) -- so `updated_at < ?` with an RFC3339 bind was
/// unconditionally true for every session regardless of actual age. Both
/// sides now come from the same `datetime()` family so they compare in the
/// same format.
pub async fn idle_session_ids(
    host_pool: &SqlitePool,
    idle_minutes: i64,
    active_minutes: i64,
) -> Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT id FROM sessions WHERE \
         (status = 'idle' AND updated_at < datetime('now', printf('-%d minutes', ?))) OR \
         (status = 'active' AND updated_at < datetime('now', printf('-%d minutes', ?)))",
    )
    .bind(idle_minutes)
    .bind(active_minutes)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn short_tool_content_is_untouched() {
        let content = "small result";
        assert_eq!(mask_tool_content("tool", content), content);
    }

    #[test]
    fn non_tool_roles_are_never_masked_regardless_of_length() {
        let content = "x".repeat(TOOL_MASK_HEAD + TOOL_MASK_TAIL + 500);
        assert_eq!(mask_tool_content("user", &content), content);
        assert_eq!(mask_tool_content("assistant", &content), content);
    }

    #[test]
    fn long_tool_content_is_elided_in_the_middle() {
        let content = "x".repeat(TOOL_MASK_HEAD + TOOL_MASK_TAIL + 500);
        let masked = mask_tool_content("tool", &content);
        assert!(masked.len() < content.len());
        assert!(masked.contains("bytes elided"));
        assert!(masked.starts_with(&"x".repeat(TOOL_MASK_HEAD)));
        assert!(masked.ends_with(&"x".repeat(TOOL_MASK_TAIL)));
    }

    #[test]
    fn elision_never_panics_on_a_multibyte_char_boundary() {
        // A multi-byte UTF-8 sequence straddling the head/tail cut points --
        // this used to be exactly what naive byte-index slicing panics on.
        let mut content = "é".repeat(TOOL_MASK_HEAD); // 2 bytes each, so this
        content.push_str(&"x".repeat(1000));
        content.push_str(&"é".repeat(TOOL_MASK_TAIL));
        // Should not panic.
        let _ = mask_tool_content("tool", &content);
    }

    #[tokio::test]
    async fn read_unlearned_chunk_masks_large_tool_output_end_to_end() {
        let options = sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:").unwrap();
        let pool = SqlitePool::connect_with(options).await.unwrap();
        sqlx::query(
            "CREATE TABLE messages (session_id TEXT, role TEXT, content TEXT, \
             rowid INTEGER PRIMARY KEY AUTOINCREMENT)",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("INSERT INTO messages (session_id, role, content) VALUES (?, ?, ?)")
            .bind("s1")
            .bind("user")
            .bind("please read this file")
            .execute(&pool)
            .await
            .unwrap();
        let huge_tool_output = "y".repeat(5000);
        sqlx::query("INSERT INTO messages (session_id, role, content) VALUES (?, ?, ?)")
            .bind("s1")
            .bind("tool")
            .bind(&huge_tool_output)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO messages (session_id, role, content) VALUES (?, ?, ?)")
            .bind("s1")
            .bind("assistant")
            .bind("here's a summary of the file")
            .execute(&pool)
            .await
            .unwrap();

        let chunk = read_unlearned_chunk(&pool, "s1", 0, 3).await.unwrap();
        assert!(chunk.contains("please read this file"));
        assert!(chunk.contains("here's a summary of the file"));
        assert!(
            chunk.contains("bytes elided"),
            "a 5000-byte tool result must be elided, not dominate the chunk verbatim"
        );
        assert!(
            !chunk.contains(&huge_tool_output),
            "the full unmasked tool output must not appear in the learn chunk"
        );
    }
}
