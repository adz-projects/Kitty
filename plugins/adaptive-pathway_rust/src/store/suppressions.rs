//! `suppressions` table access. Backed by the ported EdgeTTL semantics
//! (`learning/ttl.py`) generalized to behavioral beliefs.

use chrono::{DateTime, Utc};

use super::Db;
use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SuppressReason {
    Wrong,
    Outdated,
    Private,
    Duplicate,
}

impl SuppressReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            SuppressReason::Wrong => "wrong",
            SuppressReason::Outdated => "outdated",
            SuppressReason::Private => "private",
            SuppressReason::Duplicate => "duplicate",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Suppression {
    pub id: String,
    pub belief_id: Option<String>,
    pub text_hash: String,
    pub reason: SuppressReason,
    pub permanent: bool,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl Db {
    pub async fn insert_suppression(&self, s: &Suppression) -> Result<()> {
        sqlx::query(
            "INSERT INTO suppressions (id, belief_id, text_hash, reason, permanent, expires_at, \
             created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&s.id)
        .bind(&s.belief_id)
        .bind(&s.text_hash)
        .bind(s.reason.as_str())
        .bind(s.permanent)
        .bind(s.expires_at)
        .bind(s.created_at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// True if `text`'s hash is actively suppressed (permanent, or not-yet-expired).
    pub async fn is_text_suppressed(&self, text_hash: &str, now: DateTime<Utc>) -> Result<bool> {
        let row: Option<(i64, Option<DateTime<Utc>>, i64)> = sqlx::query_as(
            "SELECT permanent, expires_at, COUNT(*) FROM suppressions WHERE text_hash = ?",
        )
        .bind(text_hash)
        .fetch_optional(self.pool())
        .await?;
        match row {
            None => Ok(false),
            Some((permanent, expires_at, _)) => {
                if permanent != 0 {
                    return Ok(true);
                }
                match expires_at {
                    Some(exp) => Ok(exp > now),
                    None => Ok(true), // non-permanent w/o expiry treated as active
                }
            }
        }
    }

    pub async fn has_permanent_tombstone(&self, text_hash: &str) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM suppressions WHERE text_hash = ? AND permanent = 1",
        )
        .bind(text_hash)
        .fetch_one(self.pool())
        .await?
            > 0)
    }

    /// Prune expired (non-permanent) suppressions. Called by maintenance.
    pub async fn prune_expired(&self, now: DateTime<Utc>) -> Result<u64> {
        let res = sqlx::query("DELETE FROM suppressions WHERE permanent = 0 AND expires_at IS NOT NULL AND expires_at <= ?")
            .bind(now)
            .execute(self.pool())
            .await?;
        Ok(res.rows_affected())
    }
}
