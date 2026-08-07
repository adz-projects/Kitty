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
}

impl Db {
    pub async fn insert_observation(&self, o: &Observation) -> Result<()> {
        sqlx::query(
            "INSERT INTO observations (id, belief_id, session_id, statement, provenance, layer, \
             domain, evidence, contradicts, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        .execute(self.pool())
        .await?;
        Ok(())
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
            }
        })
        .collect()
}
