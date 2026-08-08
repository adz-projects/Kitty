//! `observations` table access. Raw extractions before merge/consolidation,
//! kept as an audit trail.

use chrono::{DateTime, Utc};
use sqlx::Row;

use super::Db;
use crate::error::Result;

#[derive(Debug, Clone)]
pub struct Observation {
    pub id: String,
    pub belief_id: Option<String>,
    pub session_id: Option<String>,
    pub statement: String,
    pub provenance: String,
    pub layer: String,
    pub domain: Option<String>,
    pub evidence: Option<String>,
    pub contradicts: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Shared by every observation from one `learn::extract_and_record` pass,
    /// so recall can pull co-occurring facts in behind an anchor belief.
    /// `None` for observations predating migration 006 and for single
    /// observations recorded via the `record` MCP tool -- see that migration
    /// for why this lives here rather than on `beliefs`.
    pub batch_id: Option<String>,
}

/// Cap on sibling pairs returned by `cooccurring_belief_pairs`. Bounds the
/// work a pathological batch history can create in the per-turn recall path;
/// well above what `MAX_CANDIDATES` (64) candidates can actually consume.
const COOCCURRENCE_PAIR_LIMIT: i64 = 512;

impl Db {
    pub async fn insert_observation(&self, o: &Observation) -> Result<()> {
        sqlx::query(
            "INSERT INTO observations (id, belief_id, session_id, statement, provenance, layer, \
             domain, evidence, contradicts, created_at, batch_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&o.id)
        .bind(&o.belief_id)
        .bind(&o.session_id)
        .bind(&o.statement)
        .bind(&o.provenance)
        .bind(&o.layer)
        .bind(&o.domain)
        .bind(&o.evidence)
        .bind(&o.contradicts)
        .bind(o.created_at)
        .bind(&o.batch_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Distinct pairs of belief ids that were observed together in at least
    /// one extraction batch, restricted to `belief_ids` (the recall candidate
    /// set). Returned unordered-but-deduplicated as `a < b` so callers get
    /// each undirected edge once.
    ///
    /// Built as a self-join on `observations.batch_id` rather than a stored
    /// edge table: the relation is already fully implied by the observation
    /// rows, and a separate table would need its own consistency handling on
    /// belief merge/delete for no gain. Empty input short-circuits without
    /// touching the DB, since a parameterless `IN ()` is not valid SQL.
    pub async fn cooccurring_belief_pairs(
        &self,
        belief_ids: &[String],
    ) -> Result<Vec<(String, String)>> {
        if belief_ids.len() < 2 {
            return Ok(vec![]);
        }
        let placeholders = vec!["?"; belief_ids.len()].join(", ");
        let sql = format!(
            "SELECT DISTINCT a.belief_id AS a_id, b.belief_id AS b_id \
             FROM observations a \
             JOIN observations b ON a.batch_id = b.batch_id AND a.belief_id < b.belief_id \
             WHERE a.batch_id IS NOT NULL \
             AND a.belief_id IN ({placeholders}) AND b.belief_id IN ({placeholders}) \
             LIMIT ?"
        );
        let mut q = sqlx::query(&sql);
        // Bound twice: once for each `IN` list.
        for _ in 0..2 {
            for id in belief_ids {
                q = q.bind(id);
            }
        }
        let rows = q.bind(COOCCURRENCE_PAIR_LIMIT).fetch_all(self.pool()).await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let a: Option<String> = row.try_get("a_id").ok()?;
                let b: Option<String> = row.try_get("b_id").ok()?;
                Some((a?, b?))
            })
            .collect())
    }

    pub async fn list_for_belief(&self, belief_id: &str) -> Result<Vec<Observation>> {
        let rows = sqlx::query("SELECT * FROM observations WHERE belief_id = ?")
            .bind(belief_id)
            .fetch_all(self.pool())
            .await?;
        Ok(map_observations(rows))
    }

    pub async fn list_for_session(&self, session_id: &str) -> Result<Vec<Observation>> {
        let rows = sqlx::query("SELECT * FROM observations WHERE session_id = ?")
            .bind(session_id)
            .fetch_all(self.pool())
            .await?;
        Ok(map_observations(rows))
    }

    pub async fn delete_for_belief(&self, belief_id: &str) -> Result<u64> {
        let res = sqlx::query("DELETE FROM observations WHERE belief_id = ?")
            .bind(belief_id)
            .execute(self.pool())
            .await?;
        Ok(res.rows_affected())
    }
}

fn map_observations(rows: Vec<sqlx::sqlite::SqliteRow>) -> Vec<Observation> {
    rows.into_iter()
        .map(|row| {
            Observation {
                id: row.try_get("id").unwrap_or_default(),
                belief_id: row.try_get("belief_id").unwrap_or_default(),
                session_id: row.try_get("session_id").unwrap_or_default(),
                statement: row.try_get("statement").unwrap_or_default(),
                provenance: row.try_get("provenance").unwrap_or_default(),
                layer: row.try_get("layer").unwrap_or_default(),
                domain: row.try_get("domain").unwrap_or_default(),
                evidence: row.try_get("evidence").unwrap_or_default(),
                contradicts: row.try_get("contradicts").unwrap_or_default(),
                created_at: row.try_get("created_at").unwrap_or_else(|_| Utc::now()),
                batch_id: row.try_get("batch_id").unwrap_or_default(),
            }
        })
        .collect()
}
