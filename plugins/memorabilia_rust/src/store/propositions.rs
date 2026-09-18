//! Level 1 proposition rows, chunk→proposition support links, and their
//! SQL primitives (plan §5.2). Row types and parameterized SQL only — the
//! §8 confidence math that recomputes `confidence`/`is_disputed` lives in
//! the phases that own it, and uses these primitives in the same
//! transaction as its cause.

use sqlx::FromRow;

use crate::error::Result;
use crate::store::Db;
use super::chunks::Chunk;

#[derive(Debug, Clone, FromRow)]
pub struct Proposition {
    pub node_id: String,
    pub claim: String,
    /// Derived strictly from ACTIVE supporting chunks (plan §8); stored
    /// denormalized for the retrieval hot path.
    pub confidence: f64,
    /// Derived: true iff any supporting chunk has an open DISPUTED edge
    /// (plan §9.1). Stored denormalized for the same reason.
    pub is_disputed: bool,
    /// "active" | "UNSUPPORTED_ARCHIVE" (plan §5.3).
    pub status: String,
    pub importance: String,
    pub urgency: String,
    /// Earliest-deadline rule over the chunk T_expires (plan §7.2).
    pub urgency_expires_at: Option<String>,
    pub last_assessed_at: Option<String>,
    /// Set exactly when (and only when) `status = 'UNSUPPORTED_ARCHIVE'`.
    pub archived_at: Option<String>,
    pub created_at: String,
}

impl Db {
    /// Idempotent: a re-extraction of the same chunk re-derives the same
    /// deterministic `node_id`s (Phase 6), so a retry's insert is a no-op
    /// instead of a hard PK error that would wedge the chunk pending
    /// forever. Targeted `ON CONFLICT (node_id)`, never bare `OR IGNORE`
    /// (a real PK/shape bug must still surface).
    pub async fn insert_proposition(&self, p: &Proposition) -> Result<u64> {
        Ok(sqlx::query(
            "INSERT INTO propositions (
                node_id, claim, confidence, is_disputed, status, importance,
                urgency, urgency_expires_at, last_assessed_at, archived_at,
                created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (node_id) DO NOTHING",
        )
        .bind(&p.node_id)
        .bind(&p.claim)
        .bind(p.confidence)
        .bind(p.is_disputed)
        .bind(&p.status)
        .bind(&p.importance)
        .bind(&p.urgency)
        .bind(&p.urgency_expires_at)
        .bind(&p.last_assessed_at)
        .bind(&p.archived_at)
        .bind(&p.created_at)
        .execute(self.pool())
        .await?
        .rows_affected())
    }

    pub async fn get_proposition(&self, node_id: &str) -> Result<Option<Proposition>> {
        Ok(sqlx::query_as::<_, Proposition>(
            "SELECT * FROM propositions WHERE node_id = ?",
        )
        .bind(node_id)
        .fetch_optional(self.pool())
        .await?)
    }

    pub async fn list_propositions_by_status(
        &self,
        status: &str,
    ) -> Result<Vec<Proposition>> {
        Ok(sqlx::query_as::<_, Proposition>(
            "SELECT * FROM propositions WHERE status = ? ORDER BY rowid",
        )
        .bind(status)
        .fetch_all(self.pool())
        .await?)
    }

    /// Idempotent support link (a chunk retried through extraction must not
    /// double-support). Timestamps come from the caller, never the clock —
    /// deterministic test state.
    pub async fn add_support_link(
        &self,
        chunk_id: &str,
        node_id: &str,
        created_at: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO chunk_propositions (chunk_id, node_id, created_at)
             VALUES (?, ?, ?)
             ON CONFLICT (chunk_id, node_id) DO NOTHING",
        )
        .bind(chunk_id)
        .bind(node_id)
        .bind(created_at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn remove_support_link(&self, chunk_id: &str, node_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM chunk_propositions WHERE chunk_id = ? AND node_id = ?")
            .bind(chunk_id)
            .bind(node_id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// The chunk's ACTIVE supporting chunks (input to §8 confidence and to
    /// retrieval rendering).
    pub async fn list_active_supporting_chunks(&self, node_id: &str) -> Result<Vec<Chunk>> {
        Ok(sqlx::query_as::<_, Chunk>(
            "SELECT c.* FROM chunks AS c
             JOIN chunk_propositions AS cp ON cp.chunk_id = c.chunk_id
             WHERE cp.node_id = ? AND c.status = 'active'
             ORDER BY c.rowid",
        )
        .bind(node_id)
        .fetch_all(self.pool())
        .await?)
    }

    /// The chunk's ACTIVE propositions (the Stage 4 conflict prompt renders
    /// each probe neighbor's extracted claims — plan §3.5: neighbors go in
    /// with their text AND their extracted propositions, so the LLM compares
    /// claim against claim). Archived/unsupported propositions are excluded:
    /// they are forensic, not live.
    pub async fn list_propositions_for_chunk(
        &self,
        chunk_id: &str,
    ) -> Result<Vec<Proposition>> {
        Ok(sqlx::query_as::<_, Proposition>(
            "SELECT p.* FROM propositions AS p
             JOIN chunk_propositions AS cp ON cp.node_id = p.node_id
             WHERE cp.chunk_id = ? AND p.status = 'active'
             ORDER BY p.rowid",
        )
        .bind(chunk_id)
        .fetch_all(self.pool())
        .await?)
    }

    /// COUNT(DISTINCT source_entity) over active support links — N_active
    /// in the §8.2 formula.
    pub async fn count_active_sources(&self, node_id: &str) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT COUNT(DISTINCT c.source_entity)
             FROM chunks AS c
             JOIN chunk_propositions AS cp ON cp.chunk_id = c.chunk_id
             WHERE cp.node_id = ? AND c.status = 'active'",
        )
        .bind(node_id)
        .fetch_one(self.pool())
        .await?)
    }

    pub async fn set_proposition_confidence(
        &self,
        node_id: &str,
        confidence: f64,
        is_disputed: bool,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE propositions SET confidence = ?, is_disputed = ? WHERE node_id = ?",
        )
        .bind(confidence)
        .bind(is_disputed)
        .bind(node_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Hard delete (plan §7.4 / §12.2 cascade): FK cascades remove the
    /// supporting chunk links and every edge incident to the node.
    pub async fn delete_proposition(&self, node_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM propositions WHERE node_id = ?")
            .bind(node_id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// The propositions `chunk_id` supports (the §12.2 cascade needs this
    /// list *before* deleting the chunk row, whose links cascade away).
    pub async fn list_nodes_supported_by_chunk(
        &self,
        chunk_id: &str,
    ) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT node_id FROM chunk_propositions WHERE chunk_id = ? ORDER BY node_id",
        )
        .bind(chunk_id)
        .fetch_all(self.pool())
        .await?)
    }

    /// The §5.3 proposition transition, pair-atomic; archiving here is the
    /// UNSUPPORTED_ARCHIVE step (hard delete happens later in maintenance,
    /// plan §7.4).
    pub async fn archive_proposition(
        &self,
        node_id: &str,
        archived_at: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE propositions SET status = 'UNSUPPORTED_ARCHIVE', archived_at = ? \
             WHERE node_id = ?",
        )
        .bind(archived_at)
        .bind(node_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }
}
