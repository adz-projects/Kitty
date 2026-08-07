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

    /// Forget a belief by forgiving a textual description of it (the model
    /// echoes statements, never UUIDs). Resolves `what` against the store.
    ///
    /// `reason == "wrong"` (default): permanent suppression + contradictions
    /// row at `resolved_b` + tombstone so extraction can't relearn it.
    /// `"outdated"`: 90-day suppression (may re-earn confidence).
    /// `"private"`: hard delete belief + observations + assumption + FTS row,
    /// plus a permanent `forget_tombstones` row keyed on text hash.
    ///
    /// Returns the exact statement text dropped (for the model to echo).
    pub async fn forget_by_text(
        &self,
        what: &str,
        session_recall_ids: &[String],
        reason: SuppressReason,
    ) -> Result<Option<String>> {
        let now = Utc::now();
        // resolve: prefer session recall ids (exact/substring), else top-1
        // cosine above 0.80.
        let target: Option<crate::store::beliefs::Belief> = {
            let by_known_id = self
                .lookup_beliefs_in(session_recall_ids)
                .await?;
            match by_known_id
                .into_iter()
                .find(|b| b.text.to_lowercase().contains(&what.to_lowercase()))
            {
                Some(b) => Some(b),
                None => self.best_cosine_match(what).await?,
            }
        };

        let Some(belief) = target else {
            // Nothing resolved; still record the request as an audit event.
            self.audit("forget", Some(&format!("unresolved: {what}"))).await.ok();
            return Ok(None);
        };

        let text_hash = crate::belief::synthesis::text_hash(&belief.text);

        match reason {
            SuppressReason::Wrong | SuppressReason::Duplicate => {
                self.insert_suppression(&Suppression {
                    id: crate::store::audit::uuid_string(),
                    belief_id: Some(belief.id.clone()),
                    text_hash: text_hash.clone(),
                    reason,
                    permanent: true,
                    expires_at: None,
                    created_at: now,
                })
                .await?;
                // tombstone so extraction can't relearn it
                sqlx::query("INSERT OR IGNORE INTO forget_tombstones (text_hash, created_at) VALUES (?, ?)")
                    .bind(&text_hash)
                    .bind(now)
                    .execute(self.pool())
                    .await?;
                self.audit("forget", Some(&format!("permanent: {}", belief.text))).await.ok();
            }
            SuppressReason::Outdated => {
                let expires_at = now + chrono::Duration::days(90);
                self.insert_suppression(&Suppression {
                    id: crate::store::audit::uuid_string(),
                    belief_id: Some(belief.id.clone()),
                    text_hash: text_hash.clone(),
                    reason,
                    permanent: false,
                    expires_at: Some(expires_at),
                    created_at: now,
                })
                .await?;
                self.audit("forget", Some(&format!("outdated(90d): {}", belief.text))).await.ok();
            }
            SuppressReason::Private => {
                self.delete_belief(&belief.id).await?;
                sqlx::query("DELETE FROM observations WHERE belief_id = ?")
                    .bind(&belief.id)
                    .execute(self.pool())
                    .await?;
                sqlx::query("DELETE FROM assumptions WHERE belief_id = ?")
                    .bind(&belief.id)
                    .execute(self.pool())
                    .await?;
                sqlx::query("INSERT OR IGNORE INTO forget_tombstones (text_hash, created_at) VALUES (?, ?)")
                    .bind(&text_hash)
                    .bind(now)
                    .execute(self.pool())
                    .await?;
                self.audit("forget", Some(&format!("hard_delete: {}", belief.text))).await.ok();
            }
        }

        Ok(Some(belief.text.clone()))
    }

    /// Fetch beliefs whose ids are in `ids`.
    async fn lookup_beliefs_in(
        &self,
        ids: &[String],
    ) -> Result<Vec<crate::store::beliefs::Belief>> {
        let mut out = Vec::new();
        for id in ids {
            if let Some(b) = self.get_belief(id).await? {
                out.push(b);
            }
        }
        Ok(out)
    }

    /// Top-1 belief by embedding cosine to `what`'s hash-embedding, above 0.80.
    async fn best_cosine_match(&self, what: &str) -> Result<Option<crate::store::beliefs::Belief>> {
        let all = self.list_beliefs(None).await?;
        let q = crate::embed::hashing::hash_embed(what, 384);
        let mut best: Option<(f64, crate::store::beliefs::Belief)> = None;
        for b in all {
            let cos = crate::vector::ops::cosine(&b.embedding, &q);
            if cos >= 0.80
                && best.as_ref().map(|(c, _)| cos > *c).unwrap_or(true) {
                    best = Some((cos, b));
                }
        }
        Ok(best.map(|(_, b)| b))
    }
}
