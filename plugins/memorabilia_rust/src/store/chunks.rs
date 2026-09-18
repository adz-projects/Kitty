//! Level 0 chunk rows + SQL primitives (plan §5.1).
//!
//! Row types and parameterized SQL only — no business logic. Decay
//! decisions, dispute derivations, and lifecycle transitions are made by
//! the phases that own them (§7/§9); they use these primitives inside
//! `Db::run_in_transaction`.

use sqlx::FromRow;

use crate::error::Result;
use crate::store::Db;

#[derive(Debug, Clone, FromRow)]
pub struct Chunk {
    pub chunk_id: String,
    pub content: String,
    pub content_hash: String,
    pub document_hash: String,
    pub source_entity: String,
    pub source_reliability: f64,
    pub provenance_cluster_id: String,
    pub cluster_citation: String,
    /// "active" | "archived" (CHECK-enforced; "disputed" is derived, never
    /// a status — plan §5.3).
    pub status: String,
    /// "pending" | "done" (Stage 4 extraction, plan §3.6).
    pub extraction_status: String,
    /// Last failed extraction attempt (plan §3.6, migration 003): the
    /// per-chunk retry-backoff anchor. `None` = healthy; set on failure,
    /// cleared with `extraction_status → 'done'` on success. ISO-8601 UTC
    /// strings compare lexicographically = chronologically.
    pub extraction_error_at: Option<String>,
    /// Model tag of the vector space that produced `chunks_vec`'s row for
    /// this chunk (plan §3.3): the pinned semantic model or
    /// `__lexical_hash__`.
    pub embedding_model: String,
    /// "static" | "transient" | "deadline" (plan §5.1). The τ time
    /// constants are Config knobs, not row data.
    pub decay_class: String,
    /// t_anchor: last grounding / ingest time (§7.1).
    pub anchor_at: String,
    /// T_expire, set exactly when `decay_class = 'deadline'` (CHECK).
    pub urgency_expires_at: Option<String>,
    pub reinforcement_count: i64,
    pub grounding_count: i64,
    /// Set exactly when (and only when) `status = 'archived'` (CHECK).
    pub archived_at: Option<String>,
    pub created_at: String,
}

impl Db {
    /// Insert a chunk row. The row's `status`/`archived_at` and
    /// `decay_class`/`urgency_expires_at` pairings are enforced by CHECK
    /// constraints; a mismatched write is a bug surfaced at the boundary.
    pub async fn insert_chunk(&self, c: &Chunk) -> Result<()> {
        sqlx::query(
            "INSERT INTO chunks (
                chunk_id, content, content_hash, document_hash, source_entity,
                source_reliability, provenance_cluster_id, cluster_citation,
                status, extraction_status, extraction_error_at, embedding_model,
                decay_class, anchor_at, urgency_expires_at,
                reinforcement_count, grounding_count, archived_at, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&c.chunk_id)
        .bind(&c.content)
        .bind(&c.content_hash)
        .bind(&c.document_hash)
        .bind(&c.source_entity)
        .bind(c.source_reliability)
        .bind(&c.provenance_cluster_id)
        .bind(&c.cluster_citation)
        .bind(&c.status)
        .bind(&c.extraction_status)
        .bind(&c.extraction_error_at)
        .bind(&c.embedding_model)
        .bind(&c.decay_class)
        .bind(&c.anchor_at)
        .bind(&c.urgency_expires_at)
        .bind(c.reinforcement_count)
        .bind(c.grounding_count)
        .bind(&c.archived_at)
        .bind(&c.created_at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn get_chunk(&self, chunk_id: &str) -> Result<Option<Chunk>> {
        Ok(sqlx::query_as::<_, Chunk>("SELECT * FROM chunks WHERE chunk_id = ?")
            .bind(chunk_id)
            .fetch_optional(self.pool())
            .await?)
    }

    pub async fn get_chunk_by_content_hash(
        &self,
        content_hash: &str,
    ) -> Result<Option<Chunk>> {
        Ok(sqlx::query_as::<_, Chunk>(
            "SELECT * FROM chunks WHERE content_hash = ?",
        )
        .bind(content_hash)
        .fetch_optional(self.pool())
        .await?)
    }

    pub async fn list_chunks_by_status(&self, status: &str) -> Result<Vec<Chunk>> {
        Ok(sqlx::query_as::<_, Chunk>("SELECT * FROM chunks WHERE status = ? ORDER BY rowid")
            .bind(status)
            .fetch_all(self.pool())
            .await?)
    }

    /// The Stage 4 drain: oldest pending chunks first (FIFO on rowid,
    /// plan §3.6).
    /// Total active chunks (the Stage 4 probe's over-fetch bound: one
    /// bounded brute-force vector pass, never a per-neighbor SQL scan —
    /// plan §3.5).
    pub async fn count_active_chunks(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE status = 'active'")
            .fetch_one(self.pool())
            .await?)
    }

    /// The chunk's storage rowid (the extraction watermark is a rowid
    /// bound — plan §3.6; rowid is not a column, so it comes back via a
    /// dedicated scalar query).
    pub async fn chunk_rowid(&self, chunk_id: &str) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar("SELECT rowid FROM chunks WHERE chunk_id = ?")
            .bind(chunk_id)
            .fetch_optional(self.pool())
            .await?)
    }

    pub async fn list_pending_chunks(&self, limit: i64) -> Result<Vec<Chunk>> {
        Ok(sqlx::query_as::<_, Chunk>(
            "SELECT * FROM chunks WHERE extraction_status = 'pending' ORDER BY rowid LIMIT ?",
        )
        .bind(limit)
        .fetch_all(self.pool())
        .await?)
    }

    pub async fn mark_extraction_done(&self, chunk_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE chunks SET extraction_status = 'done' WHERE chunk_id = ?",
        )
        .bind(chunk_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Record a failed extraction attempt (plan §3.6): the chunk stays
    /// `pending`, the failure timestamp starts its `retry_backoff_s`
    /// backoff. Idempotent in effect — retrying a failed chunk just moves
    /// the clock.
    pub async fn mark_extraction_failed(&self, chunk_id: &str, extraction_error_at: &str) -> Result<()> {
        sqlx::query(
            "UPDATE chunks SET extraction_error_at = ? WHERE chunk_id = ?",
        )
        .bind(extraction_error_at)
        .bind(chunk_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Finish a chunk's extraction in one pair-atomic write (plan §5.1):
    /// the Stage 4 decay profile (CHECK-enforced deadline/T_expire pairing)
    /// lands together with `extraction_status = 'done'` and the failure
    /// clock clearing — a chunk can never be seen as done-with-the-wrong
    /// profile, or done-while-still-in-backoff.
    pub async fn finish_extraction(
        &self,
        chunk_id: &str,
        decay_class: &str,
        urgency_expires_at: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE chunks
             SET extraction_status = 'done',
                 decay_class = ?,
                 urgency_expires_at = ?,
                 extraction_error_at = NULL
             WHERE chunk_id = ?",
        )
        .bind(decay_class)
        .bind(urgency_expires_at)
        .bind(chunk_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// The Stage 4 drain with retry backoff (plan §3.6): oldest pending
    /// chunks first (FIFO on rowid), excluding any still inside its
    /// `retry_backoff_s` after a failure. `ready_before` is the caller's
    /// `now - retry_backoff_s` rendered as the same ISO-8601 UTC string
    /// format (lexicographic = chronological).
    pub async fn list_ready_pending_chunks(
        &self,
        limit: i64,
        ready_before: &str,
    ) -> Result<Vec<Chunk>> {
        Ok(sqlx::query_as::<_, Chunk>(
            "SELECT * FROM chunks
             WHERE extraction_status = 'pending'
               AND (extraction_error_at IS NULL OR extraction_error_at <= ?)
             ORDER BY rowid LIMIT ?",
        )
        .bind(ready_before)
        .bind(limit)
        .fetch_all(self.pool())
        .await?)
    }

    /// Hard delete (plan §12.2 `private` / §7.4 retention pass): FK
    /// cascades remove the chunk's support links, DISPUTED edges, and
    /// outbox rows in the same statement — the chunk's `chunks_vec` row is
    /// dropped separately through the `VectorIndex` seam (the vtab has no
    /// FKs), inside the caller's transaction.
    pub async fn delete_chunk(&self, chunk_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM chunks WHERE chunk_id = ?")
            .bind(chunk_id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Retag a chunk's vector space (plan §3.3 `reembed_stale` writes the
    /// new vector through `VectorIndex::upsert`, which keeps this in
    /// lockstep; this exists for callers that replace the vector row out of
    /// band).
    pub async fn update_chunk_embedding_model(
        &self,
        chunk_id: &str,
        embedding_model: &str,
    ) -> Result<()> {
        sqlx::query("UPDATE chunks SET embedding_model = ? WHERE chunk_id = ?")
            .bind(embedding_model)
            .bind(chunk_id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Archive a chunk: sets the status/archived_at pair in one write (the
    /// CHECK constraints make the pair atomic in schema terms). Archiving is
    /// terminal until hard delete (plan §5.3) — there is no reactivation
    /// primitive by design.
    pub async fn archive_chunk(&self, chunk_id: &str, archived_at: &str) -> Result<()> {
        sqlx::query("UPDATE chunks SET status = 'archived', archived_at = ? WHERE chunk_id = ?")
            .bind(archived_at)
            .bind(chunk_id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Promote a chunk's decay class (plan §6.1 transient → static). The
    /// maintenance heavy pass calls this only after the four promotion gates
    /// hold; a static chunk decays on `τ_static` instead of `τ_transient`.
    /// Never touches `urgency_expires_at` (only a `deadline` class carries
    /// one, and promotion is transient → static).
    pub async fn set_chunk_decay_class(&self, chunk_id: &str, decay_class: &str) -> Result<()> {
        sqlx::query("UPDATE chunks SET decay_class = ? WHERE chunk_id = ? AND decay_class != 'deadline'")
            .bind(decay_class)
            .bind(chunk_id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Counter updates for the outbox drain (plan §6.2): `grounded`/
    /// `reinforced` increments and an optional anchor reset, all in one
    /// statement. Deadline chunks must never have their anchor reset — that
    /// gate lives with the drain worker, not here.
    pub async fn apply_chunk_counters(
        &self,
        chunk_id: &str,
        grounding: i64,
        reinforcement: i64,
        anchor_at: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE chunks
             SET grounding_count = grounding_count + ?,
                 reinforcement_count = reinforcement_count + ?,
                 anchor_at = COALESCE(?, anchor_at)
             WHERE chunk_id = ?",
        )
        .bind(grounding)
        .bind(reinforcement)
        .bind(anchor_at.map(|s| s.to_string()))
        .bind(chunk_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }
}
