//! `assumptions` table access. Assumptions are beliefs below the
//! surfaced/tested boundary scheduled for testing.

use chrono::{DateTime, Utc};
use sqlx::Row;

use super::Db;
use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssumptionState {
    Scheduled,
    Surfaced,
    Passed,
    Failed,
    Stale,
}

impl AssumptionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            AssumptionState::Scheduled => "scheduled",
            AssumptionState::Surfaced => "surfaced",
            AssumptionState::Passed => "passed",
            AssumptionState::Failed => "failed",
            AssumptionState::Stale => "stale",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Assumption {
    pub id: String,
    pub belief_id: Option<String>,
    pub text: String,
    pub confidence: f64,
    pub state: AssumptionState,
    /// The global exchange counter's value when this was flagged (a fixed
    /// anchor, never re-stamped) -- see `Db::global_exchange_count`. Elapsed
    /// exchanges are computed live as `current - flagged_at_exchange`.
    pub flagged_at_exchange: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn row_to_assumption(row: &sqlx::sqlite::SqliteRow) -> sqlx::Result<Assumption> {
    let id: String = row.try_get("id")?;
    let belief_id: Option<String> = row.try_get("belief_id")?;
    let text: String = row.try_get("text")?;
    let confidence: f64 = row.try_get("confidence")?;
    let state: String = row.try_get("state")?;
    let flagged_at_exchange: i64 = row.try_get("flagged_at_exchange")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at")?;
    Ok(Assumption {
        id,
        belief_id,
        text,
        confidence,
        state: match state.as_str() {
            "surfaced" => AssumptionState::Surfaced,
            "passed" => AssumptionState::Passed,
            "failed" => AssumptionState::Failed,
            "stale" => AssumptionState::Stale,
            _ => AssumptionState::Scheduled,
        },
        flagged_at_exchange,
        created_at,
        updated_at,
    })
}

impl Db {
    pub async fn insert_assumption(&self, a: &Assumption) -> Result<()> {
        sqlx::query(
            "INSERT INTO assumptions (id, belief_id, text, confidence, state, \
             flagged_at_exchange, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&a.id)
        .bind(&a.belief_id)
        .bind(&a.text)
        .bind(a.confidence)
        .bind(a.state.as_str())
        .bind(a.flagged_at_exchange)
        .bind(a.created_at)
        .bind(a.updated_at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn list_assumptions(&self, state: Option<AssumptionState>) -> Result<Vec<Assumption>> {
        let rows = if let Some(s) = state {
            sqlx::query("SELECT * FROM assumptions WHERE state = ?")
                .bind(s.as_str())
                .fetch_all(self.pool())
                .await?
        } else {
            sqlx::query("SELECT * FROM assumptions").fetch_all(self.pool()).await?
        };
        rows.iter().map(row_to_assumption).collect::<sqlx::Result<Vec<_>>>().map_err(Into::into)
    }

    /// Only `scheduled` + `surfaced` assumptions -- the two "still live,
    /// worth checking" states -- ordered oldest-flagged first so the
    /// longest-untested assumption surfaces before newer ones.
    pub async fn list_live_assumptions(&self) -> Result<Vec<Assumption>> {
        let rows = sqlx::query(
            "SELECT * FROM assumptions WHERE state IN ('scheduled', 'surfaced') \
             ORDER BY flagged_at_exchange ASC",
        )
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_assumption).collect::<sqlx::Result<Vec<_>>>().map_err(Into::into)
    }

    /// Advance an assumption's state without touching its anchor
    /// (`flagged_at_exchange` is fixed at flag time, never re-stamped).
    pub async fn set_assumption_state(&self, id: &str, state: AssumptionState) -> Result<()> {
        sqlx::query("UPDATE assumptions SET state = ?, updated_at = ? WHERE id = ?")
            .bind(state.as_str())
            .bind(Utc::now())
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn get_assumption(&self, id: &str) -> Result<Option<Assumption>> {
        let row = sqlx::query("SELECT * FROM assumptions WHERE id = ?")
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        row.as_ref().map(row_to_assumption).transpose().map_err(Into::into)
    }

    /// The (first) assumption row tracking `belief_id`, if any. Used to
    /// avoid inserting duplicate assumption rows for the same belief across
    /// repeated flagging/re-evaluation passes, and to resolve an assumption
    /// when new evidence touches its belief.
    pub async fn get_assumption_for_belief(&self, belief_id: &str) -> Result<Option<Assumption>> {
        let row = sqlx::query("SELECT * FROM assumptions WHERE belief_id = ? LIMIT 1")
            .bind(belief_id)
            .fetch_optional(self.pool())
            .await?;
        row.as_ref().map(row_to_assumption).transpose().map_err(Into::into)
    }

    /// Only a `scheduled`/`surfaced` (i.e. still-live) assumption tracking
    /// `belief_id`, if any -- used by resolution paths that should only act
    /// on a *pending* assumption, not one already passed/failed/stale.
    pub async fn get_live_assumption_for_belief(&self, belief_id: &str) -> Result<Option<Assumption>> {
        let row = sqlx::query(
            "SELECT * FROM assumptions WHERE belief_id = ? AND state IN ('scheduled', 'surfaced') LIMIT 1",
        )
        .bind(belief_id)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(row_to_assumption).transpose().map_err(Into::into)
    }
}
