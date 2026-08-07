//! `contradictions` table access. Contradictions are preserved, never
//! silently resolved -- the ×0.5 weight + uncertainty surfacing is the
//! mechanism, not deletion.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::Row;

use super::Db;
use crate::error::Result;

#[derive(Debug, Clone, Serialize)]
pub struct Contradiction {
    pub id: String,
    pub belief_a: String,
    pub belief_b: String,
    pub status: String,
    pub resolved_b: Option<String>,
    pub reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
}

impl Db {
    pub async fn insert_contradiction(&self, c: &Contradiction) -> Result<()> {
        sqlx::query(
            "INSERT INTO contradictions (id, belief_a, belief_b, status, resolved_b, reason, \
             created_at, resolved_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&c.id)
        .bind(&c.belief_a)
        .bind(&c.belief_b)
        .bind(&c.status)
        .bind(&c.resolved_b)
        .bind(&c.reason)
        .bind(c.created_at)
        .bind(c.resolved_at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn list_open(&self) -> Result<Vec<Contradiction>> {
        let rows = sqlx::query("SELECT * FROM contradictions WHERE status = 'open'")
            .fetch_all(self.pool())
            .await?;
        let mut out = Vec::new();
        for row in rows {
            let id: String = row.try_get("id")?;
            let belief_a: String = row.try_get("belief_a")?;
            let belief_b: String = row.try_get("belief_b")?;
            let status: String = row.try_get("status")?;
            let resolved_b: Option<String> = row.try_get("resolved_b")?;
            let reason: Option<String> = row.try_get("reason")?;
            let created_at: DateTime<Utc> = row.try_get("created_at")?;
            let resolved_at: Option<DateTime<Utc>> = row.try_get("resolved_at")?;
            out.push(Contradiction {
                id,
                belief_a,
                belief_b,
                status,
                resolved_b,
                reason,
                created_at,
                resolved_at,
            });
        }
        Ok(out)
    }

    pub async fn resolve(&self, id: &str, prefer_b: String) -> Result<()> {
        sqlx::query(
            "UPDATE contradictions SET status = 'resolved', resolved_b = ?, resolved_at = ? \
             WHERE id = ?",
        )
        .bind(prefer_b)
        .bind(Utc::now())
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }
}
