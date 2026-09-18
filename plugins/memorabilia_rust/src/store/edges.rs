//! Edge rows + SQL primitives: proposition-level edges (plan §9.2) and the
//! single symmetric DISPUTED chunk edge (plan §9.1). Row types and
//! parameterized SQL only.
//!
//! DISPUTED edges are written in canonical order (`chunk_a < chunk_b`,
//! CHECK-enforced) so a symmetric pair has exactly one representation; the
//! UNIQUE constraint on top makes a double-open an idempotent no-op
//! (Phase 6 re-extraction retries re-send pairs the chunk already opened).
//! Ordering the endpoints before the call is the caller's job — the CHECK
//! turns an unordered write into a visible error instead of a duplicate.

use sqlx::FromRow;

use crate::error::Result;
use crate::store::Db;

#[derive(Debug, Clone, FromRow)]
pub struct PropositionEdge {
    pub edge_id: String,
    /// DEPENDS_ON | CAUSES | BLOCKS | MODIFIES_DEADLINE | RHYMES_WITH.
    pub edge_type: String,
    pub from_node: String,
    pub to_node: String,
    /// Pass 2 propagation weight (§9.2; ≤ 0.8 keeps activation in [0,1],
    /// §10.1).
    pub weight: f64,
    /// RHYMES_WITH TTL pair (plan §9.3); NULL for the other types.
    pub revalidate_at: Option<String>,
    pub ttl_days: Option<i64>,
    pub created_at: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct DisputedEdge {
    pub edge_id: String,
    /// Canonical order: always chunk_a < chunk_b (CHECK).
    pub chunk_a: String,
    pub chunk_b: String,
    /// LLM confidence that a direct contradiction exists (>
    /// dispute_strength_floor, §9.1).
    pub strength: f64,
    pub opened_at: String,
    /// Set when either endpoint archives — closed edges impose no
    /// dispute (plan §9.1).
    pub closed_at: Option<String>,
    pub reason: String,
}

impl Db {
    pub async fn insert_proposition_edge(&self, e: &PropositionEdge) -> Result<()> {
        sqlx::query(
            "INSERT INTO proposition_edges (
                edge_id, edge_type, from_node, to_node, weight,
                revalidate_at, ttl_days, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&e.edge_id)
        .bind(&e.edge_type)
        .bind(&e.from_node)
        .bind(&e.to_node)
        .bind(e.weight)
        .bind(&e.revalidate_at)
        .bind(e.ttl_days)
        .bind(&e.created_at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn delete_proposition_edge(&self, edge_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM proposition_edges WHERE edge_id = ?")
            .bind(edge_id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn list_proposition_edges_from(&self, node_id: &str) -> Result<Vec<PropositionEdge>> {
        Ok(sqlx::query_as::<_, PropositionEdge>(
            "SELECT * FROM proposition_edges WHERE from_node = ? ORDER BY rowid",
        )
        .bind(node_id)
        .fetch_all(self.pool())
        .await?)
    }

    pub async fn list_proposition_edges_to(&self, node_id: &str) -> Result<Vec<PropositionEdge>> {
        Ok(sqlx::query_as::<_, PropositionEdge>(
            "SELECT * FROM proposition_edges WHERE to_node = ? ORDER BY rowid",
        )
        .bind(node_id)
        .fetch_all(self.pool())
        .await?)
    }

    pub async fn list_proposition_edges_of_type(
        &self,
        edge_type: &str,
    ) -> Result<Vec<PropositionEdge>> {
        Ok(sqlx::query_as::<_, PropositionEdge>(
            "SELECT * FROM proposition_edges WHERE edge_type = ? ORDER BY rowid",
        )
        .bind(edge_type)
        .fetch_all(self.pool())
        .await?)
    }

    /// True iff the (undirected) pair already carries `edge_type` — the
    /// RHYMES_WITH "skip unordered pairs that already carry an edge" gate
    /// (plan §9.3).
    pub async fn has_proposition_edge(
        &self,
        edge_type: &str,
        a: &str,
        b: &str,
    ) -> Result<bool> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM proposition_edges
             WHERE edge_type = ? AND ((from_node = ? AND to_node = ?)
                   OR (from_node = ? AND to_node = ?))",
        )
        .bind(edge_type)
        .bind(a)
        .bind(b)
        .bind(b)
        .bind(a)
        .fetch_one(self.pool())
        .await?;
        Ok(n > 0)
    }

    // ----------------------------------------------------------------
    // DISPUTED edges (plan §9.1)
    // ----------------------------------------------------------------

    /// Open a DISPUTED edge. Callers MUST order the endpoints
    /// (min, max); the CHECK rejects a mis-ordered write at the schema
    /// boundary. Re-opening an existing (open or closed) pair is an
    /// idempotent no-op via the targeted UNIQUE constraint (plan §9.1 /
    /// §15 Phase 6: a re-extraction retry must not fail on the pair it
    /// already opened).
    pub async fn insert_disputed_edge(&self, e: &DisputedEdge) -> Result<u64> {
        Ok(sqlx::query(
            "INSERT INTO disputed_edges (
                edge_id, chunk_a, chunk_b, strength, opened_at, closed_at, reason
             ) VALUES (?, ?, ?, ?, ?, NULL, ?)
             ON CONFLICT (chunk_a, chunk_b) DO NOTHING",
        )
        .bind(&e.edge_id)
        .bind(&e.chunk_a)
        .bind(&e.chunk_b)
        .bind(e.strength)
        .bind(&e.opened_at)
        .bind(&e.reason)
        .execute(self.pool())
        .await?
        .rows_affected())
    }

    /// Close an open edge (endpoint archive, plan §9.1); idempotent —
    /// closing an already-closed edge touches nothing.
    pub async fn close_disputed_edge(&self, edge_id: &str, closed_at: &str) -> Result<()> {
        sqlx::query(
            "UPDATE disputed_edges SET closed_at = ? WHERE edge_id = ? AND closed_at IS NULL",
        )
        .bind(closed_at)
        .bind(edge_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Open DISPUTED edges incident to a chunk (both endpoint positions —
    /// the canonical order means an endpoint is either `chunk_a` or
    /// `chunk_b`).
    pub async fn list_open_disputes_for_chunk(
        &self,
        chunk_id: &str,
    ) -> Result<Vec<DisputedEdge>> {
        Ok(sqlx::query_as::<_, DisputedEdge>(
            "SELECT * FROM disputed_edges
             WHERE closed_at IS NULL AND (chunk_a = ? OR chunk_b = ?)
             ORDER BY rowid",
        )
        .bind(chunk_id)
        .bind(chunk_id)
        .fetch_all(self.pool())
        .await?)
    }

    /// All open edges (the maintenance close pass and the §8 dispute
    /// strength derivation iterate these).
    pub async fn list_open_disputed_edges(&self) -> Result<Vec<DisputedEdge>> {
        Ok(sqlx::query_as::<_, DisputedEdge>(
            "SELECT * FROM disputed_edges WHERE closed_at IS NULL ORDER BY rowid",
        )
        .fetch_all(self.pool())
        .await?)
    }

    /// The chunk's `dispute_strength` input: MAX(strength) over its open
    /// edges, or None when disputed-free (§8).
    pub async fn max_open_dispute_strength(&self, chunk_id: &str) -> Result<Option<f64>> {
        Ok(sqlx::query_scalar(
            "SELECT MAX(strength) FROM disputed_edges
             WHERE closed_at IS NULL AND (chunk_a = ? OR chunk_b = ?)",
        )
        .bind(chunk_id)
        .bind(chunk_id)
        .fetch_one(self.pool())
        .await?)
    }
}
