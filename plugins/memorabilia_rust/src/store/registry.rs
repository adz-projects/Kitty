//! Per-source reliability registry rows + SQL primitives (plan §4.2).
//! One row per `source_entity`: the tier, its prior, and the drifted
//! reliability score used as `source_reliability` for new chunks.
//! Seeding (once, clamp(tier_prior × intent_factor)) and drifting
//! (±reliability_band) are the callers' decisions; this module only stores.

use sqlx::FromRow;

use crate::error::Result;
use crate::store::Db;

#[derive(Debug, Clone, FromRow)]
pub struct Source {
    pub source_entity: String,
    /// "primary" | "established" | "community" | "personal".
    pub tier: String,
    /// Snapshot of the data-file prior at seed time (drift stays anchored
    /// to it even if reliability.yaml is later revised).
    pub tier_prior: f64,
    /// Current drifted value; seed = clamp(tier_prior × intent_factor).
    pub reliability: f64,
    pub last_drift_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl Db {
    /// Seed-once: the first sight of a source creates the row; a repeated
    /// seed (re-ingest, retry) touches nothing — re-seeding would erase
    /// accumulated drift (plan §4.2/§4.3: "drift only").
    pub async fn seed_source(
        &self,
        source_entity: &str,
        tier: &str,
        tier_prior: f64,
        reliability: f64,
        at: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO source_registry (
                source_entity, tier, tier_prior, reliability,
                last_drift_at, created_at, updated_at
             ) VALUES (?, ?, ?, ?, NULL, ?, ?)
             ON CONFLICT (source_entity) DO NOTHING",
        )
        .bind(source_entity)
        .bind(tier)
        .bind(tier_prior)
        .bind(reliability)
        .bind(at)
        .bind(at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn get_source(&self, source_entity: &str) -> Result<Option<Source>> {
        Ok(sqlx::query_as::<_, Source>(
            "SELECT * FROM source_registry WHERE source_entity = ?",
        )
        .bind(source_entity)
        .fetch_optional(self.pool())
        .await?)
    }

    /// Drift primitive, used at the end of each maintenance cycle (plan
    /// §4.3): applies one bounded step in the caller-supplied direction.
    pub async fn drift_source_reliability(
        &self,
        source_entity: &str,
        reliability: f64,
        at: &str,
    ) -> Result<()> {
        // Clamp to the valid reliability range, mirroring the seed-time
        // `clamp(tier_prior × intent_factor)` invariant. The step direction
        // and magnitude are the caller's, but the stored score is bounded
        // here so an out-of-range value can never persist.
        let reliability = reliability.clamp(0.0, 1.0);
        sqlx::query(
            "UPDATE source_registry
             SET reliability = ?, last_drift_at = ?, updated_at = ?
             WHERE source_entity = ?",
        )
        .bind(reliability)
        .bind(at)
        .bind(at)
        .bind(source_entity)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn list_sources(&self) -> Result<Vec<Source>> {
        Ok(sqlx::query_as::<_, Source>("SELECT * FROM source_registry ORDER BY rowid")
            .fetch_all(self.pool())
            .await?)
    }
}
