//! `domains` table access.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::Row;

use super::Db;
use crate::error::Result;

#[derive(Debug, Clone, Serialize)]
pub struct Domain {
    pub id: String,
    pub name: String,
    pub centroid: Option<Vec<f32>>,
    pub dpp_diversity_weight: f64,
    pub novelty_lambda: f64,
    pub sessions: i64,
    pub belief_count: i64,
    pub last_inferred: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl Db {
    pub async fn upsert_domain(&self, d: &Domain) -> Result<()> {
        sqlx::query(
            "INSERT INTO domains (id, name, centroid, dpp_diversity_weight, novelty_lambda, \
             sessions, belief_count, last_inferred, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET name=excluded.name, centroid=excluded.centroid, \
             dpp_diversity_weight=excluded.dpp_diversity_weight, novelty_lambda=excluded.novelty_lambda, \
             sessions=excluded.sessions, belief_count=excluded.belief_count, \
             last_inferred=excluded.last_inferred",
        )
        .bind(&d.id)
        .bind(&d.name)
        .bind(d.centroid.as_ref().map(|c| super::encode_embedding(c)))
        .bind(d.dpp_diversity_weight)
        .bind(d.novelty_lambda)
        .bind(d.sessions)
        .bind(d.belief_count)
        .bind(d.last_inferred)
        .bind(d.created_at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn list_domains(&self) -> Result<Vec<Domain>> {
        let rows = sqlx::query("SELECT * FROM domains")
            .fetch_all(self.pool())
            .await?;
        let mut out = Vec::new();
        for row in rows {
            let id: String = row.try_get("id")?;
            let name: String = row.try_get("name")?;
            let centroid_blob: Option<Vec<u8>> = row.try_get("centroid")?;
            let dpp_diversity_weight: f64 = row.try_get("dpp_diversity_weight")?;
            let novelty_lambda: f64 = row.try_get("novelty_lambda")?;
            let sessions: i64 = row.try_get("sessions")?;
            let belief_count: i64 = row.try_get("belief_count")?;
            let last_inferred: Option<DateTime<Utc>> = row.try_get("last_inferred")?;
            let created_at: DateTime<Utc> = row.try_get("created_at")?;
            out.push(Domain {
                id,
                name,
                centroid: centroid_blob.map(|b| super::decode_embedding(&b)),
                dpp_diversity_weight,
                novelty_lambda,
                sessions,
                belief_count,
                last_inferred,
                created_at,
            });
        }
        Ok(out)
    }
}
