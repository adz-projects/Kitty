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
    pub exchanged_since_flag: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Db {
    pub async fn insert_assumption(&self, a: &Assumption) -> Result<()> {
        sqlx::query(
            "INSERT INTO assumptions (id, belief_id, text, confidence, state, \
             exchanged_since_flag, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&a.id)
        .bind(&a.belief_id)
        .bind(&a.text)
        .bind(a.confidence)
        .bind(a.state.as_str())
        .bind(a.exchanged_since_flag)
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
        let mut out = Vec::new();
        for row in rows {
            let id: String = row.try_get("id")?;
            let belief_id: Option<String> = row.try_get("belief_id")?;
            let text: String = row.try_get("text")?;
            let confidence: f64 = row.try_get("confidence")?;
            let state: String = row.try_get("state")?;
            let exchanged_since_flag: i64 = row.try_get("exchanged_since_flag")?;
            let created_at: DateTime<Utc> = row.try_get("created_at")?;
            let updated_at: DateTime<Utc> = row.try_get("updated_at")?;
            out.push(Assumption {
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
                exchanged_since_flag,
                created_at,
                updated_at,
            });
        }
        Ok(out)
    }

    pub async fn update_assumption_state(&self, id: &str, state: AssumptionState, exchanged: i64) -> Result<()> {
        sqlx::query(
            "UPDATE assumptions SET state = ?, exchanged_since_flag = ?, updated_at = ? WHERE id = ?",
        )
        .bind(state.as_str())
        .bind(exchanged)
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
        match row {
            None => Ok(None),
            Some(row) => {
                let id: String = row.try_get("id")?;
                let belief_id: Option<String> = row.try_get("belief_id")?;
                let text: String = row.try_get("text")?;
                let confidence: f64 = row.try_get("confidence")?;
                let state: String = row.try_get("state")?;
                let exchanged_since_flag: i64 = row.try_get("exchanged_since_flag")?;
                let created_at: DateTime<Utc> = row.try_get("created_at")?;
                let updated_at: DateTime<Utc> = row.try_get("updated_at")?;
                Ok(Some(Assumption {
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
                    exchanged_since_flag,
                    created_at,
                    updated_at,
                }))
            }
        }
    }
}
