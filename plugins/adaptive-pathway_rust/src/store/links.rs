//! Typed belief links and assumption-test bookkeeping (migration 007).

use chrono::Utc;

use super::Db;
use crate::error::Result;

/// How one belief relates to another, as extraction reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// A explains or motivates B.
    Explains,
    /// A and B pull in different directions (not a flat contradiction).
    InTensionWith,
    /// A is a special case of B.
    Specializes,
}

impl Relation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Relation::Explains => "explains",
            Relation::InTensionWith => "in_tension_with",
            Relation::Specializes => "specializes",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "explains" => Some(Relation::Explains),
            "in_tension_with" => Some(Relation::InTensionWith),
            "specializes" => Some(Relation::Specializes),
            _ => None,
        }
    }
}

impl Db {
    /// Record `a -relation-> b`. Idempotent; a self-link is ignored.
    pub async fn insert_link(&self, a: &str, b: &str, relation: Relation) -> Result<()> {
        if a == b {
            return Ok(());
        }
        sqlx::query(
            "INSERT OR IGNORE INTO belief_links (belief_a, belief_b, relation, created_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(a)
        .bind(b)
        .bind(relation.as_str())
        .bind(Utc::now().to_rfc3339())
        .execute(&*self.write().await)
        .await?;
        Ok(())
    }

    /// Every link among `ids` (both ends in the set), as `(a, b, relation)`.
    pub async fn links_among(&self, ids: &[String]) -> Result<Vec<(String, String, Relation)>> {
        if ids.len() < 2 {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "SELECT belief_a, belief_b, relation FROM belief_links \
             WHERE belief_a IN ({marks}) AND belief_b IN ({marks})"
        );
        let mut q = sqlx::query_as::<_, (String, String, String)>(&sql);
        for id in ids.iter().chain(ids.iter()) {
            q = q.bind(id);
        }
        Ok(q.fetch_all(self.read_pool())
            .await?
            .into_iter()
            .filter_map(|(a, b, r)| Some((a, b, Relation::parse(&r)?)))
            .collect())
    }

    /// Record that `assumption_id` was put to `session_id` on the message
    /// hashed `message_hash`. One row per (assumption, chat): the first
    /// showing wins, so a later message never re-shows it.
    pub async fn record_assumption_shown(
        &self,
        assumption_id: &str,
        session_id: &str,
        message_hash: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT OR IGNORE INTO assumption_tests (assumption_id, session_id, message_hash, shown_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(assumption_id)
        .bind(session_id)
        .bind(message_hash)
        .bind(Utc::now().to_rfc3339())
        .execute(&*self.write().await)
        .await?;
        Ok(())
    }

    /// The message hash `assumption_id` was shown on in `session_id`, if it
    /// was.
    pub async fn assumption_shown_on(&self, assumption_id: &str, session_id: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT message_hash FROM assumption_tests WHERE assumption_id = ? AND session_id = ?",
        )
        .bind(assumption_id)
        .bind(session_id)
        .fetch_optional(self.read_pool())
        .await?)
    }

    /// Assumptions put to `session_id` that no extraction pass has judged
    /// yet: `(assumption_id, belief_id, text)`. The "tests in flight" the next
    /// pass is asked about.
    pub async fn tests_in_flight(&self, session_id: &str) -> Result<Vec<(String, Option<String>, String)>> {
        Ok(sqlx::query_as(
            "SELECT a.id, a.belief_id, a.text FROM assumption_tests t \
             JOIN assumptions a ON a.id = t.assumption_id \
             WHERE t.session_id = ? AND t.judged = 0 ORDER BY t.shown_at",
        )
        .bind(session_id)
        .fetch_all(self.read_pool())
        .await?)
    }

    /// Mark a test judged (whatever the outcome), so it leaves the in-flight
    /// list.
    pub async fn mark_test_judged(&self, assumption_id: &str, session_id: &str) -> Result<()> {
        sqlx::query("UPDATE assumption_tests SET judged = 1 WHERE assumption_id = ? AND session_id = ?")
            .bind(assumption_id)
            .bind(session_id)
            .execute(&*self.write().await)
            .await?;
        Ok(())
    }

    /// Open contradictions touching any of `ids`, as `(contradiction_id,
    /// belief_a, belief_b)`.
    pub async fn open_tensions_for(&self, ids: &[String]) -> Result<Vec<(String, String, String)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "SELECT id, belief_a, belief_b FROM contradictions WHERE status = 'open' \
             AND (belief_a IN ({marks}) OR belief_b IN ({marks})) ORDER BY created_at, id"
        );
        let mut q = sqlx::query_as::<_, (String, String, String)>(&sql);
        for id in ids.iter().chain(ids.iter()) {
            q = q.bind(id);
        }
        Ok(q.fetch_all(self.read_pool()).await?)
    }

    /// Whether an open or resolved contradiction already records this pair
    /// (either order).
    pub async fn has_contradiction(&self, a: &str, b: &str) -> Result<bool> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM contradictions WHERE \
             (belief_a = ? AND belief_b = ?) OR (belief_a = ? AND belief_b = ?)",
        )
        .bind(a)
        .bind(b)
        .bind(b)
        .bind(a)
        .fetch_one(self.read_pool())
        .await?;
        Ok(n > 0)
    }

    /// Resolve every open contradiction involving `belief_id` (it was
    /// corrected, forgotten, or a test settled it).
    pub async fn resolve_tensions_of(&self, belief_id: &str, reason: &str) -> Result<()> {
        sqlx::query(
            "UPDATE contradictions SET status = 'resolved', \
             reason = COALESCE(reason, '') || '; resolved: ' || ?, resolved_at = ? \
             WHERE status = 'open' AND (belief_a = ? OR belief_b = ?)",
        )
        .bind(reason)
        .bind(Utc::now())
        .bind(belief_id)
        .bind(belief_id)
        .execute(&*self.write().await)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn assumption_shown_once_per_chat_and_tracked_until_judged() {
        let db = Db::open_in_memory().await.unwrap();
        db.record_assumption_shown("a1", "s1", "h1").await.unwrap();
        db.record_assumption_shown("a1", "s1", "h2").await.unwrap();
        assert_eq!(db.assumption_shown_on("a1", "s1").await.unwrap().as_deref(), Some("h1"));
        assert_eq!(db.assumption_shown_on("a1", "s2").await.unwrap(), None);
    }
}
