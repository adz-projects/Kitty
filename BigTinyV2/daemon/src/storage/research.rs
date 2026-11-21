//! Deep research runs and their append-only ledger. See
//! `migrations/034_research.sql` for the tables and `crate::research` for what
//! the ledger means.

use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};

use crate::error::StorageError;

pub const STATUS_PLANNING: &str = "planning";
pub const STATUS_RESEARCHING: &str = "researching";
pub const STATUS_CRITIQUING: &str = "critiquing";
pub const STATUS_WRITING: &str = "writing";
pub const STATUS_DONE: &str = "done";
pub const STATUS_FAILED: &str = "failed";
pub const STATUS_CANCELLED: &str = "cancelled";

/// Statuses a run can still leave on its own.
pub const LIVE_STATUSES: [&str; 4] = [
    STATUS_PLANNING,
    STATUS_RESEARCHING,
    STATUS_CRITIQUING,
    STATUS_WRITING,
];

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct RunRow {
    pub id: String,
    pub app_id: String,
    pub session_id: Option<String>,
    pub brief: String,
    pub status: String,
    pub max_turns: i64,
    pub turns_used: i64,
    pub contract: Option<String>,
    pub outline: Option<String>,
    pub tokens_used: i64,
    pub output_path: Option<String>,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
}

impl RunRow {
    pub fn is_live(&self) -> bool {
        LIVE_STATUSES.contains(&self.status.as_str())
    }
}

const RUN_COLS: &str = "id, app_id, session_id, brief, status, max_turns, turns_used, contract, \
                        outline, tokens_used, output_path, error, created_at, updated_at, \
                        finished_at";

#[derive(Debug, Clone, FromRow, Serialize, Deserialize, PartialEq)]
pub struct EntryRow {
    pub seq: i64,
    pub wave: i64,
    pub kind: String,
    pub payload: String,
    pub norm_key: Option<String>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize, PartialEq)]
pub struct LinkRow {
    pub from_seq: i64,
    pub to_seq: i64,
    pub kind: String,
    pub reason: String,
    pub wave: i64,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize, PartialEq)]
pub struct TagRow {
    pub seq: i64,
    pub tag_kind: String,
    pub tag: String,
    pub wave: i64,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize, PartialEq)]
pub struct StepRow {
    pub turn: i64,
    pub phase: String,
    pub session_id: Option<String>,
    pub status: String,
    pub summary: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
}

/// What appending an entry did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appended {
    /// A new row with this seq.
    New(i64),
    /// An exact repeat (same kind and `norm_key`) of the row with this seq.
    Existing(i64),
}

impl Appended {
    pub fn seq(self) -> i64 {
        match self {
            Appended::New(s) | Appended::Existing(s) => s,
        }
    }
}

// ---------------------------------------------------------------- runs

pub async fn create_run(
    pool: &SqlitePool,
    id: &str,
    app_id: &str,
    session_id: &str,
    brief: &str,
    max_turns: i64,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO research_runs (id, app_id, session_id, brief, max_turns) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(app_id)
    .bind(session_id)
    .bind(brief)
    .bind(max_turns)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_run(pool: &SqlitePool, id: &str) -> Result<Option<RunRow>, StorageError> {
    let sql = format!("SELECT {RUN_COLS} FROM research_runs WHERE id = ?");
    Ok(sqlx::query_as::<_, RunRow>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// One run, if this app owns it.
pub async fn get_run_for_app(
    pool: &SqlitePool,
    id: &str,
    app_id: &str,
) -> Result<Option<RunRow>, StorageError> {
    let sql = format!("SELECT {RUN_COLS} FROM research_runs WHERE id = ? AND app_id = ?");
    Ok(sqlx::query_as::<_, RunRow>(&sql)
        .bind(id)
        .bind(app_id)
        .fetch_optional(pool)
        .await?)
}

/// This app's runs, newest first, optionally only those of one chat.
pub async fn list_runs_for_app(
    pool: &SqlitePool,
    app_id: &str,
    session_id: Option<&str>,
    limit: i64,
) -> Result<Vec<RunRow>, StorageError> {
    let limit = limit.clamp(1, 500);
    let sql = format!(
        "SELECT {RUN_COLS} FROM research_runs \
         WHERE app_id = ?1 AND (?2 IS NULL OR session_id = ?2) \
         ORDER BY created_at DESC, rowid DESC LIMIT ?3"
    );
    Ok(sqlx::query_as::<_, RunRow>(&sql)
        .bind(app_id)
        .bind(session_id)
        .bind(limit)
        .fetch_all(pool)
        .await?)
}

/// Runs a previous daemon left unfinished, oldest first, for resumption.
pub async fn list_live_runs(pool: &SqlitePool) -> Result<Vec<RunRow>, StorageError> {
    let sql = format!(
        "SELECT {RUN_COLS} FROM research_runs \
         WHERE status IN ('planning','researching','critiquing','writing') \
         ORDER BY created_at, rowid"
    );
    Ok(sqlx::query_as::<_, RunRow>(&sql).fetch_all(pool).await?)
}

/// A live chat may hold at most one live run.
pub async fn live_run_for_session(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<Option<RunRow>, StorageError> {
    let sql = format!(
        "SELECT {RUN_COLS} FROM research_runs WHERE session_id = ? \
         AND status IN ('planning','researching','critiquing','writing') \
         ORDER BY created_at DESC LIMIT 1"
    );
    Ok(sqlx::query_as::<_, RunRow>(&sql)
        .bind(session_id)
        .fetch_optional(pool)
        .await?)
}

/// Move a live run to `status`. Terminal statuses also stamp `finished_at`
/// and, for `failed`/`cancelled`, record `error`. A run that already ended
/// stays as it is: returns `false` then, so a late writer cannot resurrect a
/// cancelled run.
pub async fn set_status(
    pool: &SqlitePool,
    id: &str,
    status: &str,
    error: Option<&str>,
) -> Result<bool, StorageError> {
    let terminal = !LIVE_STATUSES.contains(&status);
    let res = sqlx::query(
        "UPDATE research_runs SET status = ?2, \
             error = COALESCE(?3, error), \
             updated_at = datetime('now'), \
             finished_at = CASE WHEN ?4 THEN datetime('now') ELSE finished_at END \
         WHERE id = ?1 AND status IN ('planning','researching','critiquing','writing')",
    )
    .bind(id)
    .bind(status)
    .bind(error)
    .bind(terminal)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn set_contract(
    pool: &SqlitePool,
    id: &str,
    contract_json: &str,
    max_turns: i64,
) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE research_runs SET contract = ?2, max_turns = ?3, updated_at = datetime('now') \
         WHERE id = ?1",
    )
    .bind(id)
    .bind(contract_json)
    .bind(max_turns)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_outline(
    pool: &SqlitePool,
    id: &str,
    outline_json: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE research_runs SET outline = ?2, updated_at = datetime('now') WHERE id = ?1",
    )
    .bind(id)
    .bind(outline_json)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_output_path(pool: &SqlitePool, id: &str, path: &str) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE research_runs SET output_path = ?2, updated_at = datetime('now') WHERE id = ?1",
    )
    .bind(id)
    .bind(path)
    .execute(pool)
    .await?;
    Ok(())
}

/// Count a finished turn and the tokens it spent.
pub async fn add_turn(pool: &SqlitePool, id: &str, tokens: i64) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE research_runs SET turns_used = turns_used + 1, \
             tokens_used = tokens_used + ?2, updated_at = datetime('now') WHERE id = ?1",
    )
    .bind(id)
    .bind(tokens.max(0))
    .execute(pool)
    .await?;
    Ok(())
}

/// Tokens spent outside a counted turn (turn 0, a retried step).
pub async fn add_tokens(pool: &SqlitePool, id: &str, tokens: i64) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE research_runs SET tokens_used = tokens_used + ?2, updated_at = datetime('now') \
         WHERE id = ?1",
    )
    .bind(id)
    .bind(tokens.max(0))
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- ledger

/// Append one entry. An entry whose `(kind, norm_key)` already exists is not
/// written again; the existing seq comes back as [`Appended::Existing`].
///
/// The seq is allocated inside the INSERT itself, which SQLite runs under its
/// write lock, so concurrent appends to one run cannot draw the same number.
pub async fn append_entry(
    pool: &SqlitePool,
    run_id: &str,
    wave: i64,
    kind: &str,
    payload: &str,
    norm_key: Option<&str>,
) -> Result<Appended, StorageError> {
    let inserted: Option<(i64,)> = sqlx::query_as(
        "INSERT OR IGNORE INTO research_entries (run_id, seq, wave, kind, payload, norm_key) \
         SELECT ?1, COALESCE(MAX(seq), 0) + 1, ?2, ?3, ?4, ?5 \
         FROM research_entries WHERE run_id = ?1 \
         RETURNING seq",
    )
    .bind(run_id)
    .bind(wave)
    .bind(kind)
    .bind(payload)
    .bind(norm_key)
    .fetch_optional(pool)
    .await?;
    if let Some((seq,)) = inserted {
        return Ok(Appended::New(seq));
    }
    let Some(key) = norm_key else {
        return Err(StorageError::Generic(format!(
            "research entry for run {run_id} was not written"
        )));
    };
    let existing: Option<(i64,)> = sqlx::query_as(
        "SELECT seq FROM research_entries WHERE run_id = ? AND kind = ? AND norm_key = ?",
    )
    .bind(run_id)
    .bind(kind)
    .bind(key)
    .fetch_optional(pool)
    .await?;
    existing
        .map(|(seq,)| Appended::Existing(seq))
        .ok_or_else(|| StorageError::Generic(format!("research entry for run {run_id} vanished")))
}

/// Record a link. Repeating an existing link is a no-op. Returns whether a
/// new link was written. Both ends must be entries of this run.
pub async fn add_link(
    pool: &SqlitePool,
    run_id: &str,
    from_seq: i64,
    to_seq: i64,
    kind: &str,
    reason: &str,
    wave: i64,
) -> Result<bool, StorageError> {
    if from_seq == to_seq {
        return Ok(false);
    }
    let res = sqlx::query(
        "INSERT OR IGNORE INTO research_links (run_id, from_seq, to_seq, kind, reason, wave) \
         SELECT ?1, ?2, ?3, ?4, ?5, ?6 \
         WHERE (SELECT COUNT(*) FROM research_entries \
                WHERE run_id = ?1 AND seq IN (?2, ?3)) = 2",
    )
    .bind(run_id)
    .bind(from_seq)
    .bind(to_seq)
    .bind(kind)
    .bind(reason)
    .bind(wave)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Place an entry against a contract cell or section. Idempotent.
pub async fn add_tag(
    pool: &SqlitePool,
    run_id: &str,
    seq: i64,
    tag_kind: &str,
    tag: &str,
    wave: i64,
) -> Result<bool, StorageError> {
    let res = sqlx::query(
        "INSERT OR IGNORE INTO research_tags (run_id, seq, tag_kind, tag, wave) \
         SELECT ?1, ?2, ?3, ?4, ?5 \
         WHERE EXISTS (SELECT 1 FROM research_entries WHERE run_id = ?1 AND seq = ?2)",
    )
    .bind(run_id)
    .bind(seq)
    .bind(tag_kind)
    .bind(tag)
    .bind(wave)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn entries(pool: &SqlitePool, run_id: &str) -> Result<Vec<EntryRow>, StorageError> {
    Ok(sqlx::query_as::<_, EntryRow>(
        "SELECT seq, wave, kind, payload, norm_key FROM research_entries \
         WHERE run_id = ? ORDER BY seq",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?)
}

pub async fn links(pool: &SqlitePool, run_id: &str) -> Result<Vec<LinkRow>, StorageError> {
    Ok(sqlx::query_as::<_, LinkRow>(
        "SELECT from_seq, to_seq, kind, reason, wave FROM research_links \
         WHERE run_id = ? ORDER BY rowid",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?)
}

pub async fn tags(pool: &SqlitePool, run_id: &str) -> Result<Vec<TagRow>, StorageError> {
    Ok(sqlx::query_as::<_, TagRow>(
        "SELECT seq, tag_kind, tag, wave FROM research_tags WHERE run_id = ? ORDER BY rowid",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?)
}

// ---------------------------------------------------------------- steps

/// Start (or restart, after a daemon restart) the step for `turn`.
pub async fn start_step(
    pool: &SqlitePool,
    run_id: &str,
    turn: i64,
    phase: &str,
    session_id: Option<&str>,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO research_steps (run_id, turn, phase, session_id) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(run_id, turn) DO UPDATE SET phase = ?3, session_id = ?4, \
             status = 'running', summary = NULL, started_at = datetime('now'), finished_at = NULL",
    )
    .bind(run_id)
    .bind(turn)
    .bind(phase)
    .bind(session_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn finish_step(
    pool: &SqlitePool,
    run_id: &str,
    turn: i64,
    status: &str,
    summary: Option<&str>,
) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE research_steps SET status = ?3, summary = ?4, finished_at = datetime('now') \
         WHERE run_id = ?1 AND turn = ?2",
    )
    .bind(run_id)
    .bind(turn)
    .bind(status)
    .bind(summary)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn steps(pool: &SqlitePool, run_id: &str) -> Result<Vec<StepRow>, StorageError> {
    Ok(sqlx::query_as::<_, StepRow>(
        "SELECT turn, phase, session_id, status, summary, started_at, finished_at \
         FROM research_steps WHERE run_id = ? ORDER BY turn",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        crate::storage::sessions::create_session(&pool, "s1", "Chat")
            .await
            .unwrap();
        create_run(&pool, "r1", "kitty", "s1", "brief", 4)
            .await
            .unwrap();
        pool
    }

    #[tokio::test]
    async fn seqs_count_up_and_exact_repeats_land_on_the_first_row() {
        let pool = pool().await;
        let a = append_entry(&pool, "r1", 1, "source", "{}", Some("example.com/a"))
            .await
            .unwrap();
        let b = append_entry(&pool, "r1", 1, "finding", "{}", Some("k1"))
            .await
            .unwrap();
        let again = append_entry(&pool, "r1", 2, "source", "{\"x\":1}", Some("example.com/a"))
            .await
            .unwrap();
        let unkeyed = append_entry(&pool, "r1", 2, "lead", "{}", None)
            .await
            .unwrap();
        let unkeyed2 = append_entry(&pool, "r1", 2, "lead", "{}", None)
            .await
            .unwrap();
        assert_eq!(a, Appended::New(1));
        assert_eq!(b, Appended::New(2));
        assert_eq!(again, Appended::Existing(1));
        assert_eq!(unkeyed, Appended::New(3));
        assert_eq!(unkeyed2, Appended::New(4));
        // The same key under another kind is a different entry.
        let other_kind = append_entry(&pool, "r1", 2, "finding", "{}", Some("example.com/a"))
            .await
            .unwrap();
        assert_eq!(other_kind, Appended::New(5));
        let rows = entries(&pool, "r1").await.unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows[0].payload, "{}",
            "a repeat never rewrites the first row"
        );
    }

    #[tokio::test]
    async fn the_ledger_refuses_updates() {
        let pool = pool().await;
        append_entry(&pool, "r1", 1, "finding", "{}", None)
            .await
            .unwrap();
        append_entry(&pool, "r1", 1, "source", "{}", None)
            .await
            .unwrap();
        add_link(&pool, "r1", 1, 2, "cites", "", 1).await.unwrap();
        let e = sqlx::query("UPDATE research_entries SET payload = 'x' WHERE run_id = 'r1'")
            .execute(&pool)
            .await;
        assert!(e.is_err());
        let l = sqlx::query("UPDATE research_links SET reason = 'x' WHERE run_id = 'r1'")
            .execute(&pool)
            .await;
        assert!(l.is_err());
    }

    #[tokio::test]
    async fn links_and_tags_need_entries_of_the_same_run() {
        let pool = pool().await;
        append_entry(&pool, "r1", 1, "finding", "{}", None)
            .await
            .unwrap();
        append_entry(&pool, "r1", 1, "source", "{}", None)
            .await
            .unwrap();
        assert!(add_link(&pool, "r1", 1, 2, "cites", "", 1).await.unwrap());
        assert!(
            !add_link(&pool, "r1", 1, 2, "cites", "", 2).await.unwrap(),
            "repeat"
        );
        assert!(
            !add_link(&pool, "r1", 1, 99, "cites", "", 1).await.unwrap(),
            "dangling"
        );
        assert!(
            !add_link(&pool, "r1", 1, 1, "cites", "", 1).await.unwrap(),
            "self"
        );
        assert!(add_tag(&pool, "r1", 1, "cell", "1950s", 1).await.unwrap());
        assert!(!add_tag(&pool, "r1", 42, "cell", "1950s", 1).await.unwrap());
        assert_eq!(links(&pool, "r1").await.unwrap().len(), 1);
        assert_eq!(tags(&pool, "r1").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_ended_run_cannot_be_moved_again() {
        let pool = pool().await;
        assert!(set_status(&pool, "r1", STATUS_RESEARCHING, None)
            .await
            .unwrap());
        assert!(live_run_for_session(&pool, "s1").await.unwrap().is_some());
        assert!(set_status(&pool, "r1", STATUS_CANCELLED, Some("stopped"))
            .await
            .unwrap());
        assert!(!set_status(&pool, "r1", STATUS_WRITING, None).await.unwrap());
        let run = get_run(&pool, "r1").await.unwrap().unwrap();
        assert_eq!(run.status, STATUS_CANCELLED);
        assert_eq!(run.error.as_deref(), Some("stopped"));
        assert!(run.finished_at.is_some());
        assert!(live_run_for_session(&pool, "s1").await.unwrap().is_none());
        assert!(list_live_runs(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn steps_restart_in_place_and_turns_add_up() {
        let pool = pool().await;
        start_step(&pool, "r1", 1, "wave", Some("c1"))
            .await
            .unwrap();
        start_step(&pool, "r1", 1, "wave", Some("c2"))
            .await
            .unwrap();
        finish_step(&pool, "r1", 1, "succeeded", Some("9 findings"))
            .await
            .unwrap();
        let s = steps(&pool, "r1").await.unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].session_id.as_deref(), Some("c2"));
        assert_eq!(s[0].status, "succeeded");
        add_turn(&pool, "r1", 1200).await.unwrap();
        add_tokens(&pool, "r1", 300).await.unwrap();
        let run = get_run_for_app(&pool, "r1", "kitty")
            .await
            .unwrap()
            .unwrap();
        assert_eq!((run.turns_used, run.tokens_used), (1, 1500));
        assert!(get_run_for_app(&pool, "r1", "other")
            .await
            .unwrap()
            .is_none());
    }
}
