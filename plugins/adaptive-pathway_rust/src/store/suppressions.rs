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
    ///
    /// Deliberately two scalar `COUNT(*)` queries rather than one query
    /// mixing an aggregate (`COUNT(*)`) with non-aggregate columns
    /// (`permanent`, `expires_at`) and no `GROUP BY` -- that pattern always
    /// returns exactly one row even when zero suppressions match (SQLite
    /// picks arbitrary/NULL values for the non-aggregate columns in that
    /// case), so `fetch_optional` never actually saw `None`, and a NULL
    /// `expires_at` on that phantom row fell into the "non-permanent w/o
    /// expiry treated as active" branch -- reporting *no suppression at
    /// all* as an active one.
    pub async fn is_text_suppressed(&self, text_hash: &str, now: DateTime<Utc>) -> Result<bool> {
        if self.has_permanent_tombstone(text_hash).await? {
            return Ok(true);
        }
        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM suppressions WHERE text_hash = ? AND permanent = 0 \
             AND (expires_at IS NULL OR expires_at > ?)",
        )
        .bind(text_hash)
        .bind(now)
        .fetch_one(self.pool())
        .await?;
        Ok(active > 0)
    }

    /// Text hashes of every currently-active suppression (permanent, or
    /// not-yet-expired). One query, used to filter a candidate belief list
    /// before scoring (`belief::filter_suppressed`) rather than one query
    /// per belief.
    pub async fn active_suppressed_text_hashes(
        &self,
        now: DateTime<Utc>,
    ) -> Result<std::collections::HashSet<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT text_hash FROM suppressions \
             WHERE permanent = 1 OR (expires_at IS NOT NULL AND expires_at > ?)",
        )
        .bind(now)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
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
    /// `what_embedding` is `what`'s embedding under the engine's *real*
    /// embedder (Ollama-semantic when available), computed by the caller
    /// (`Db` has no embedder of its own to call). It backs the final cosine
    /// fallback for a paraphrase that doesn't textually match anything --
    /// pass an empty slice to skip that fallback (e.g. when the caller has
    /// no embedder handy and only wants the exact/substring resolution
    /// steps; `best_cosine_match` treats an empty query as "no match").
    ///
    /// Returns the exact statement text dropped (for the model to echo).
    pub async fn forget_by_text(
        &self,
        what: &str,
        what_embedding: &[f32],
        session_recall_ids: &[String],
        reason: SuppressReason,
    ) -> Result<Option<String>> {
        let now = Utc::now();
        // resolve: prefer session recall ids (exact/substring), else
        // global text-substring, else top-1 semantic cosine above 0.80.
        let target: Option<crate::store::beliefs::Belief> = {
            let what_lower = what.to_lowercase();
            let by_known_id = self.lookup_beliefs_in(session_recall_ids).await?;
            let from_known = by_known_id
                .into_iter()
                .find(|b| b.text.to_lowercase().contains(&what_lower));
            match from_known {
                Some(b) => Some(b),
                None => {
                    // global text-substring fallback, then top-1 semantic cosine
                    match self.lookup_by_text(&what_lower).await? {
                        Some(b) => Some(b),
                        None => self.best_cosine_match(what_embedding).await?,
                    }
                }
            }
        };

        let Some(belief) = target else {
            // Nothing resolved; still record the request as an audit event.
            self.audit("forget", Some(&format!("unresolved: {what}"))).await.ok();
            return Ok(None);
        };

        self.apply_forget(belief, reason, now).await
    }

    /// Forget a belief already resolved by id (the Settings belief browser
    /// has the exact id, so none of `forget_by_text`'s fuzzy resolution is
    /// needed or appropriate here). Same reason semantics as
    /// `forget_by_text`. Returns the exact statement text dropped, or `None`
    /// if the id doesn't exist.
    pub async fn forget_belief_by_id(
        &self,
        belief_id: &str,
        reason: SuppressReason,
    ) -> Result<Option<String>> {
        let now = Utc::now();
        let Some(belief) = self.get_belief(belief_id).await? else {
            self.audit("forget", Some(&format!("unresolved id: {belief_id}"))).await.ok();
            return Ok(None);
        };
        self.apply_forget(belief, reason, now).await
    }

    /// Shared tail of both `forget_by_text` and `forget_belief_by_id`, once a
    /// belief has been resolved: apply the reason-specific suppression/
    /// tombstone/hard-delete effects and return the dropped text.
    async fn apply_forget(
        &self,
        belief: crate::store::beliefs::Belief,
        reason: SuppressReason,
        now: DateTime<Utc>,
    ) -> Result<Option<String>> {
        let text_hash = crate::belief::synthesis::text_hash(&belief.text);

        // Whatever the reason, the user acting on this belief at all --
        // correcting it, marking it stale, or asking it be forgotten --
        // resolves any live assumption tracking it as failed. Best-effort:
        // never let assumption bookkeeping block the forget itself.
        let _ = self.resolve_assumption_for_belief(&belief.id, false).await;

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

    /// Find a belief whose text contains `what_lower` (substring match).
    async fn lookup_by_text(&self, what_lower: &str) -> Result<Option<crate::store::beliefs::Belief>> {
        let all = self.list_beliefs(None).await?;
        // prefer exact/contains-match by longest shared overlap; first is fine
        // for the forget() use-case (the model echoes a full statement).
        Ok(all.into_iter().find(|b| {
            b.text.to_lowercase().contains(what_lower) || what_lower.contains(&b.text.to_lowercase())
        }))
    }

    /// Top-1 belief by embedding cosine to `query`, above 0.80. `query` must
    /// be a real embedding in the *same* space as the stored belief
    /// embeddings (the caller's `EmbeddingProvider`) -- an empty slice
    /// (norm ~0) always yields no match rather than comparing against a
    /// mismatched or degenerate vector.
    async fn best_cosine_match(&self, query: &[f32]) -> Result<Option<crate::store::beliefs::Belief>> {
        if crate::vector::ops::norm(query) < 1e-12 {
            return Ok(None);
        }
        let all = self.list_beliefs(None).await?;
        let mut best: Option<(f64, crate::store::beliefs::Belief)> = None;
        for b in all {
            let cos = crate::vector::ops::cosine(&b.embedding, query);
            if cos >= 0.80
                && best.as_ref().map(|(c, _)| cos > *c).unwrap_or(true) {
                    best = Some((cos, b));
                }
        }
        Ok(best.map(|(_, b)| b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Db;

    #[tokio::test]
    async fn no_matching_rows_is_not_suppressed() {
        let db = Db::open_in_memory().await.unwrap();
        assert!(!db.is_text_suppressed("nonexistent", Utc::now()).await.unwrap());
    }

    #[tokio::test]
    async fn permanent_suppression_is_active() {
        let db = Db::open_in_memory().await.unwrap();
        db.insert_suppression(&Suppression {
            id: "s1".into(),
            belief_id: None,
            text_hash: "h1".into(),
            reason: SuppressReason::Wrong,
            permanent: true,
            expires_at: None,
            created_at: Utc::now(),
        })
        .await
        .unwrap();
        assert!(db.is_text_suppressed("h1", Utc::now()).await.unwrap());
    }

    #[tokio::test]
    async fn expired_non_permanent_suppression_is_not_active() {
        let db = Db::open_in_memory().await.unwrap();
        db.insert_suppression(&Suppression {
            id: "s1".into(),
            belief_id: None,
            text_hash: "h1".into(),
            reason: SuppressReason::Outdated,
            permanent: false,
            expires_at: Some(Utc::now() - chrono::Duration::days(1)),
            created_at: Utc::now() - chrono::Duration::days(91),
        })
        .await
        .unwrap();
        assert!(!db.is_text_suppressed("h1", Utc::now()).await.unwrap());
    }

    #[tokio::test]
    async fn unexpired_non_permanent_suppression_is_active() {
        let db = Db::open_in_memory().await.unwrap();
        db.insert_suppression(&Suppression {
            id: "s1".into(),
            belief_id: None,
            text_hash: "h1".into(),
            reason: SuppressReason::Outdated,
            permanent: false,
            expires_at: Some(Utc::now() + chrono::Duration::days(89)),
            created_at: Utc::now(),
        })
        .await
        .unwrap();
        assert!(db.is_text_suppressed("h1", Utc::now()).await.unwrap());
    }

    #[tokio::test]
    async fn forget_by_id_resolves_directly_without_fuzzy_matching() {
        let db = Db::open_in_memory().await.unwrap();
        let now = Utc::now();
        db.insert_belief(&crate::store::beliefs::Belief {
            id: "exact-id".into(),
            text: "The user prefers dark mode.".into(),
            embedding: vec![1.0, 0.0],
            confidence: 0.7,
            provenance: crate::store::beliefs::Provenance::DirectStatement,
            layer: crate::store::beliefs::Layer::Context,
            tested: true,
            domain: None,
            tier: "context".into(),
            support_count: 1,
            distinct_sessions: 1,
            contradict_count: 0,
            pinned: false,
            last_confirmed_at: Some(now),
            consolidated_at: None,
            created_at: now,
            updated_at: now,
            session_id: None,
        })
        .await
        .unwrap();

        let dropped = db.forget_belief_by_id("exact-id", SuppressReason::Wrong).await.unwrap();
        assert_eq!(dropped.as_deref(), Some("The user prefers dark mode."));
        // `Wrong` suppresses (permanently, via text-hash tombstone) but does
        // not itself delete the belief row -- only `Private` hard-deletes.
        assert!(db.get_belief("exact-id").await.unwrap().is_some());
        let text_hash = crate::belief::synthesis::text_hash("The user prefers dark mode.");
        assert!(db.is_text_suppressed(&text_hash, Utc::now()).await.unwrap());
    }

    #[tokio::test]
    async fn forget_by_id_private_hard_deletes() {
        let db = Db::open_in_memory().await.unwrap();
        let now = Utc::now();
        db.insert_belief(&crate::store::beliefs::Belief {
            id: "exact-id".into(),
            text: "The user's home address is somewhere private.".into(),
            embedding: vec![1.0, 0.0],
            confidence: 0.7,
            provenance: crate::store::beliefs::Provenance::DirectStatement,
            layer: crate::store::beliefs::Layer::Context,
            tested: true,
            domain: None,
            tier: "context".into(),
            support_count: 1,
            distinct_sessions: 1,
            contradict_count: 0,
            pinned: false,
            last_confirmed_at: Some(now),
            consolidated_at: None,
            created_at: now,
            updated_at: now,
            session_id: None,
        })
        .await
        .unwrap();

        let dropped = db.forget_belief_by_id("exact-id", SuppressReason::Private).await.unwrap();
        assert!(dropped.is_some());
        assert!(db.get_belief("exact-id").await.unwrap().is_none(), "private reason must hard-delete the row");
    }

    #[tokio::test]
    async fn forget_by_id_unknown_id_is_none() {
        let db = Db::open_in_memory().await.unwrap();
        let dropped = db.forget_belief_by_id("nonexistent", SuppressReason::Wrong).await.unwrap();
        assert_eq!(dropped, None);
    }

    #[tokio::test]
    async fn deleting_the_row_lifts_suppression() {
        let db = Db::open_in_memory().await.unwrap();
        db.insert_suppression(&Suppression {
            id: "s1".into(),
            belief_id: None,
            text_hash: "h1".into(),
            reason: SuppressReason::Outdated,
            permanent: false,
            expires_at: Some(Utc::now() - chrono::Duration::days(1)),
            created_at: Utc::now() - chrono::Duration::days(91),
        })
        .await
        .unwrap();
        let pruned = db.prune_expired(Utc::now()).await.unwrap();
        assert_eq!(pruned, 1);
        assert!(!db.is_text_suppressed("h1", Utc::now()).await.unwrap());
    }
}
