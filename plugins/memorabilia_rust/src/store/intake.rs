//! Overnight-intake rows + SQL primitives (migration 005,
//! docs/MEMORY_OVERNIGHT.md): captures, demand queries, candidates and the
//! screen log. Row types and parameterized SQL only.

use sqlx::FromRow;

use crate::error::Result;
use crate::store::Db;

#[derive(Debug, Clone, FromRow)]
pub struct Capture {
    pub capture_id: String,
    pub document_hash: String,
    pub session_id: Option<String>,
    pub turn_id: Option<i64>,
    pub source_type: String,
    pub source_name: String,
    pub source_entity: String,
    pub intent_factor: f64,
    pub captured_at: String,
    /// "unscreened" | "screened" | "dropped" | "processed".
    pub status: String,
    pub raw_text: Option<String>,
    /// JSON array of the kept paragraphs.
    pub kept_paragraphs: Option<String>,
    pub paragraphs_total: i64,
    pub paragraphs_kept: i64,
    pub pinned: bool,
    pub screened_at: Option<String>,
}

impl Capture {
    pub fn kept(&self) -> Vec<String> {
        self.kept_paragraphs
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct Candidate {
    pub candidate_id: String,
    pub capture_id: String,
    pub session_id: Option<String>,
    pub turn_id: Option<i64>,
    pub content: String,
    pub content_hash: String,
    pub embedding: Vec<u8>,
    pub embedding_model: String,
    pub pinned: bool,
    /// "waiting" | "promoted" | "merged".
    pub status: String,
    pub created_at: String,
    pub decided_at: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct DemandQuery {
    pub id: i64,
    pub session_id: String,
    pub turn_id: i64,
    pub query: String,
    pub embedding: Option<Vec<u8>>,
    pub embedding_model: Option<String>,
    pub created_at: String,
}

/// One screen-log row as written.
#[derive(Debug, Clone)]
pub struct ScreenLogEntry {
    pub capture_id: String,
    pub source_type: String,
    pub source_name: String,
    pub heading: Option<String>,
    pub text: String,
    pub question_set: Option<String>,
    /// JSON object, question name → P(yes).
    pub scores: Option<String>,
    pub verdict: String,
    pub created_at: String,
}

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct ScreenLogRow {
    pub id: i64,
    pub capture_id: String,
    pub source_type: String,
    pub source_name: String,
    pub heading: Option<String>,
    pub text: String,
    pub question_set: Option<String>,
    pub scores: Option<String>,
    pub verdict: String,
    pub created_at: String,
}

/// Counts for Settings: what is waiting where.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct IntakeBacklog {
    pub unscreened: i64,
    pub screened: i64,
    pub candidates_waiting: i64,
    pub pending_extraction: i64,
}

impl Db {
    /// Record a capture's intake lane (`current` | `durable`, migration 007).
    pub async fn set_capture_lane(&self, capture_id: &str, lane: &str) -> Result<()> {
        sqlx::query("UPDATE captures SET lane = ? WHERE capture_id = ?")
            .bind(lane)
            .bind(capture_id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// A capture's intake lane; `durable` when unknown.
    pub async fn capture_lane(&self, capture_id: &str) -> Result<String> {
        Ok(sqlx::query_scalar::<_, String>("SELECT lane FROM captures WHERE capture_id = ?")
            .bind(capture_id)
            .fetch_optional(self.pool())
            .await?
            .unwrap_or_else(|| "durable".into()))
    }

    /// Record a chunk's intake lane.
    pub async fn set_chunk_lane(&self, chunk_id: &str, lane: &str) -> Result<()> {
        sqlx::query("UPDATE chunks SET lane = ? WHERE chunk_id = ?")
            .bind(lane)
            .bind(chunk_id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// A chunk's intake lane; `durable` when unknown.
    pub async fn chunk_lane(&self, chunk_id: &str) -> Result<String> {
        Ok(sqlx::query_scalar::<_, String>("SELECT lane FROM chunks WHERE chunk_id = ?")
            .bind(chunk_id)
            .fetch_optional(self.pool())
            .await?
            .unwrap_or_else(|| "durable".into()))
    }

    pub async fn insert_capture(&self, c: &Capture) -> Result<()> {
        sqlx::query(
            "INSERT INTO captures (
                capture_id, document_hash, session_id, turn_id, source_type,
                source_name, source_entity, intent_factor, captured_at, status,
                raw_text, kept_paragraphs, paragraphs_total, paragraphs_kept,
                pinned, screened_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(document_hash) DO NOTHING",
        )
        .bind(&c.capture_id)
        .bind(&c.document_hash)
        .bind(&c.session_id)
        .bind(c.turn_id)
        .bind(&c.source_type)
        .bind(&c.source_name)
        .bind(&c.source_entity)
        .bind(c.intent_factor)
        .bind(&c.captured_at)
        .bind(&c.status)
        .bind(&c.raw_text)
        .bind(&c.kept_paragraphs)
        .bind(c.paragraphs_total)
        .bind(c.paragraphs_kept)
        .bind(c.pinned)
        .bind(&c.screened_at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn has_capture(&self, document_hash: &str) -> Result<bool> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM captures WHERE document_hash = ?")
            .bind(document_hash)
            .fetch_one(self.pool())
            .await?;
        Ok(n > 0)
    }

    pub async fn get_capture(&self, capture_id: &str) -> Result<Option<Capture>> {
        Ok(
            sqlx::query_as::<_, Capture>("SELECT * FROM captures WHERE capture_id = ?")
                .bind(capture_id)
                .fetch_optional(self.pool())
                .await?,
        )
    }

    /// Captures in `status`, oldest first.
    pub async fn list_captures(&self, status: &str, limit: i64) -> Result<Vec<Capture>> {
        Ok(sqlx::query_as::<_, Capture>(
            "SELECT * FROM captures WHERE status = ? ORDER BY captured_at, capture_id LIMIT ?",
        )
        .bind(status)
        .bind(limit)
        .fetch_all(self.pool())
        .await?)
    }

    /// Record a screen result: the kept paragraphs replace the raw text.
    pub async fn set_capture_screened(
        &self,
        capture_id: &str,
        status: &str,
        kept: &[String],
        total: i64,
        at: &str,
    ) -> Result<()> {
        let kept_json = serde_json::to_string(kept).unwrap_or_else(|_| "[]".into());
        sqlx::query(
            "UPDATE captures SET status = ?, raw_text = NULL, kept_paragraphs = ?,
                paragraphs_total = ?, paragraphs_kept = ?, screened_at = ?
             WHERE capture_id = ?",
        )
        .bind(status)
        .bind(kept_json)
        .bind(total)
        .bind(kept.len() as i64)
        .bind(at)
        .bind(capture_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Move a capture to `status`, clearing its text (it has been chunked,
    /// or dropped).
    pub async fn close_capture(&self, capture_id: &str, status: &str) -> Result<()> {
        sqlx::query(
            "UPDATE captures SET status = ?, raw_text = NULL, kept_paragraphs = NULL
             WHERE capture_id = ?",
        )
        .bind(status)
        .bind(capture_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Drop unscreened captures captured before `cutoff`. Returns how many.
    pub async fn drop_unscreened_before(&self, cutoff: &str) -> Result<u64> {
        let r = sqlx::query(
            "UPDATE captures SET status = 'dropped', raw_text = NULL
             WHERE status = 'unscreened' AND captured_at < ?",
        )
        .bind(cutoff)
        .execute(self.pool())
        .await?;
        Ok(r.rows_affected())
    }

    pub async fn insert_demand_query(
        &self,
        session_id: &str,
        turn_id: i64,
        query: &str,
        at: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO demand_queries (session_id, turn_id, query, created_at)
             VALUES (?, ?, ?, ?) ON CONFLICT(session_id, turn_id, query) DO NOTHING",
        )
        .bind(session_id)
        .bind(turn_id)
        .bind(query)
        .bind(at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Queries not yet embedded in `model`'s space.
    pub async fn list_unembedded_queries(&self, model: &str, limit: i64) -> Result<Vec<DemandQuery>> {
        Ok(sqlx::query_as::<_, DemandQuery>(
            "SELECT * FROM demand_queries
             WHERE embedding IS NULL OR embedding_model IS NULL OR embedding_model != ?
             ORDER BY id LIMIT ?",
        )
        .bind(model)
        .bind(limit)
        .fetch_all(self.pool())
        .await?)
    }

    pub async fn set_query_embedding(&self, id: i64, embedding: &[u8], model: &str) -> Result<()> {
        sqlx::query("UPDATE demand_queries SET embedding = ?, embedding_model = ? WHERE id = ?")
            .bind(embedding)
            .bind(model)
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Embedded queries created at or after `since`.
    pub async fn list_queries_since(&self, since: &str) -> Result<Vec<DemandQuery>> {
        Ok(sqlx::query_as::<_, DemandQuery>(
            "SELECT * FROM demand_queries WHERE embedding IS NOT NULL AND created_at >= ?
             ORDER BY id",
        )
        .bind(since)
        .fetch_all(self.pool())
        .await?)
    }

    pub async fn delete_queries_before(&self, cutoff: &str) -> Result<u64> {
        let r = sqlx::query("DELETE FROM demand_queries WHERE created_at < ?")
            .bind(cutoff)
            .execute(self.pool())
            .await?;
        Ok(r.rows_affected())
    }

    pub async fn insert_candidate(&self, c: &Candidate) -> Result<bool> {
        let r = sqlx::query(
            "INSERT INTO candidates (
                candidate_id, capture_id, session_id, turn_id, content,
                content_hash, embedding, embedding_model, pinned, status,
                created_at, decided_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT DO NOTHING",
        )
        .bind(&c.candidate_id)
        .bind(&c.capture_id)
        .bind(&c.session_id)
        .bind(c.turn_id)
        .bind(&c.content)
        .bind(&c.content_hash)
        .bind(&c.embedding)
        .bind(&c.embedding_model)
        .bind(c.pinned)
        .bind(&c.status)
        .bind(&c.created_at)
        .bind(&c.decided_at)
        .execute(self.pool())
        .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn has_candidate_hash(&self, content_hash: &str) -> Result<bool> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM candidates WHERE content_hash = ?")
            .bind(content_hash)
            .fetch_one(self.pool())
            .await?;
        Ok(n > 0)
    }

    /// Waiting candidates, newest first, at most `limit`.
    pub async fn list_waiting_candidates(&self, limit: i64) -> Result<Vec<Candidate>> {
        Ok(sqlx::query_as::<_, Candidate>(
            "SELECT * FROM candidates WHERE status = 'waiting'
             ORDER BY created_at DESC, candidate_id LIMIT ?",
        )
        .bind(limit)
        .fetch_all(self.pool())
        .await?)
    }

    pub async fn set_candidate_status(&self, candidate_id: &str, status: &str, at: &str) -> Result<()> {
        sqlx::query("UPDATE candidates SET status = ?, decided_at = ? WHERE candidate_id = ?")
            .bind(status)
            .bind(at)
            .bind(candidate_id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn delete_candidate(&self, candidate_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM candidates WHERE candidate_id = ?")
            .bind(candidate_id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Delete waiting candidates created before `cutoff`. Returns how many.
    pub async fn expire_candidates_before(&self, cutoff: &str) -> Result<u64> {
        let r = sqlx::query("DELETE FROM candidates WHERE status = 'waiting' AND created_at < ?")
            .bind(cutoff)
            .execute(self.pool())
            .await?;
        Ok(r.rows_affected())
    }

    pub async fn insert_screen_log(&self, e: &ScreenLogEntry) -> Result<()> {
        sqlx::query(
            "INSERT INTO screen_log (
                capture_id, source_type, source_name, heading, text,
                question_set, scores, verdict, created_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&e.capture_id)
        .bind(&e.source_type)
        .bind(&e.source_name)
        .bind(&e.heading)
        .bind(&e.text)
        .bind(&e.question_set)
        .bind(&e.scores)
        .bind(&e.verdict)
        .bind(&e.created_at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// The newest screen-log rows, at most `limit`, newest first.
    pub async fn list_screen_log(&self, limit: i64) -> Result<Vec<ScreenLogRow>> {
        Ok(sqlx::query_as::<_, ScreenLogRow>(
            "SELECT * FROM screen_log ORDER BY id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(self.pool())
        .await?)
    }

    pub async fn delete_screen_log_before(&self, cutoff: &str) -> Result<u64> {
        let r = sqlx::query("DELETE FROM screen_log WHERE created_at < ?")
            .bind(cutoff)
            .execute(self.pool())
            .await?;
        Ok(r.rows_affected())
    }

    pub async fn intake_backlog(&self) -> Result<IntakeBacklog> {
        let count = |sql: &'static str| async move {
            sqlx::query_scalar::<_, i64>(sql).fetch_one(self.pool()).await
        };
        Ok(IntakeBacklog {
            unscreened: count("SELECT COUNT(*) FROM captures WHERE status = 'unscreened'").await?,
            screened: count("SELECT COUNT(*) FROM captures WHERE status = 'screened'").await?,
            candidates_waiting: count("SELECT COUNT(*) FROM candidates WHERE status = 'waiting'")
                .await?,
            pending_extraction: count(
                "SELECT COUNT(*) FROM chunks WHERE status = 'active' AND extraction_status = 'pending'",
            )
            .await?,
        })
    }
}
