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

/// Scan beliefs for engine-side contradictions (cosine in [0.72, 0.93] with
/// opposite polarity) and record them as open contradictions, bumping each
/// side's `contradict_count`. Called by consolidation.
///
/// Bounded to the `CONTRADICTION_SCAN_ROW_LIMIT` most-recently-touched
/// beliefs (audit #131): the pass is O(n²) pairwise cosine with a COUNT
/// query per flagged pair, so an unbounded full-table scan made every sweep
/// quadratic in total store size. Recency is the right subset — a new
/// contradiction involves at least one recently-changed belief, matching
/// the `list_recall_candidates` 500-row bound on the recall hot path.
pub async fn run_contradiction_pass(db: &Db) -> Result<()> {
    const CONTRADICTION_SCAN_ROW_LIMIT: i64 = 500;
    let beliefs = db.list_recent_beliefs(CONTRADICTION_SCAN_ROW_LIMIT).await?;
    for (i, a) in beliefs.iter().enumerate() {
        for b in beliefs.iter().skip(i + 1) {
            if crate::belief::contradiction::engine_contradiction(&a.embedding, &b.embedding) {
                let existing: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM contradictions WHERE \
                     (belief_a = ? AND belief_b = ?) OR (belief_a = ? AND belief_b = ?)",
                )
                .bind(&a.id)
                .bind(&b.id)
                .bind(&b.id)
                .bind(&a.id)
                .fetch_one(db.pool())
                .await?;
                if existing == 0 {
                    db.insert_contradiction(&Contradiction {
                        id: crate::store::audit::uuid_string(),
                        belief_a: a.id.clone(),
                        belief_b: b.id.clone(),
                        status: "open".into(),
                        resolved_b: None,
                        reason: Some("engine_detected".into()),
                        created_at: Utc::now(),
                        resolved_at: None,
                    })
                    .await?;
                    // bump contradict_count on both (no confidence update --
                    // the ×0.5 weight + uncertainty surfacing is the mechanism)
                    for id in [&a.id, &b.id] {
                        sqlx::query("UPDATE beliefs SET contradict_count = contradict_count + 1 WHERE id = ?")
                            .bind(id)
                            .execute(db.pool())
                            .await?;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::beliefs::{Belief, Layer, Provenance};

    fn belief(id: &str, embedding: Vec<f32>, updated_at: DateTime<Utc>) -> Belief {
        Belief {
            id: id.into(),
            text: format!("belief {id}"),
            embedding,
            confidence: 0.5,
            provenance: Provenance::InferredPattern,
            layer: Layer::Context,
            tested: false,
            domain: None,
            tier: "context".into(),
            support_count: 1,
            distinct_sessions: 1,
            contradict_count: 0,
            pinned: false,
            last_confirmed_at: None,
            consolidated_at: None,
            created_at: updated_at,
            updated_at,
            session_id: None,
            embedding_model: crate::config::DEFAULT_EMBEDDING_MODEL.into(),
        }
    }

    /// Cosine ≈ 0.84 (inside the [0.72, 0.93] band) with opposite mean
    /// polarity: `a` is all-positive, `b` is negative-majority.
    fn contradicting_pair() -> (Vec<f32>, Vec<f32>) {
        (vec![1.0, 0.0, 0.0, 0.0], vec![0.8, -0.3, -0.3, -0.3])
    }

    #[tokio::test]
    async fn pass_records_an_engine_contradiction_and_bumps_counts() {
        let db = Db::open_in_memory().await.unwrap();
        let now = Utc::now();
        let (ea, eb) = contradicting_pair();
        db.insert_belief(&belief("a", ea, now)).await.unwrap();
        db.insert_belief(&belief("b", eb, now)).await.unwrap();

        run_contradiction_pass(&db).await.unwrap();

        let open = db.list_open().await.unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].reason.as_deref(), Some("engine_detected"));
        for id in ["a", "b"] {
            let b = db.get_belief(id).await.unwrap().unwrap();
            assert_eq!(b.contradict_count, 1, "{id} must be bumped");
        }

        // Idempotent: a second sweep must not duplicate the row.
        run_contradiction_pass(&db).await.unwrap();
        assert_eq!(db.list_open().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn pass_is_bounded_to_the_most_recent_beliefs() {
        // Audit #131: the O(n²) sweep used to scan the full table. The
        // contradicting pair here is the *oldest* two rows; 503 newer
        // fillers push them past the 500-row scan window, so the sweep must
        // not see them.
        let db = Db::open_in_memory().await.unwrap();
        let base = Utc::now();
        let (ea, eb) = contradicting_pair();
        db.insert_belief(&belief("old-a", ea, base)).await.unwrap();
        db.insert_belief(&belief("old-b", eb, base + chrono::Duration::seconds(1)))
            .await
            .unwrap();
        for i in 0..503 {
            db.insert_belief(&belief(
                &format!("f{i}"),
                vec![0.0, 1.0, 0.0, 0.0],
                base + chrono::Duration::seconds(2 + i),
            ))
            .await
            .unwrap();
        }

        run_contradiction_pass(&db).await.unwrap();
        assert!(
            db.list_open().await.unwrap().is_empty(),
            "pairs beyond the recency cap must not be scanned"
        );
    }
}
