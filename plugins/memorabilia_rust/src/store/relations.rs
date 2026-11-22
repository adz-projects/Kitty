//! Claim vectors and claim-level relations (migration 006).

use crate::error::Result;
use crate::store::{decode_embedding, encode_embedding, Db};

/// A claim-level relation.
#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    pub from_node: String,
    pub to_node: String,
    /// `disputes` | `supersedes`.
    pub kind: String,
    pub strength: f64,
    pub reason: String,
    pub opened_at: String,
}

type RelationRow = (String, String, String, f64, String, String);

fn to_relation(r: RelationRow) -> Relation {
    Relation {
        from_node: r.0,
        to_node: r.1,
        kind: r.2,
        strength: r.3,
        reason: r.4,
        opened_at: r.5,
    }
}

impl Db {
    /// Store (or replace) a claim's embedding.
    pub async fn upsert_claim_vector(&self, node_id: &str, model: &str, v: &[f32]) -> Result<()> {
        sqlx::query(
            "INSERT INTO claim_vectors (node_id, embedding_model, embedding) VALUES (?, ?, ?) \
             ON CONFLICT (node_id) DO UPDATE SET embedding_model = excluded.embedding_model, \
             embedding = excluded.embedding",
        )
        .bind(node_id)
        .bind(model)
        .bind(encode_embedding(v))
        .execute(&*self.write().await)
        .await?;
        Ok(())
    }

    /// Every active claim's vector in `model`'s space, as `(node_id, vector)`.
    pub async fn active_claim_vectors(&self, model: &str) -> Result<Vec<(String, Vec<f32>)>> {
        let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(
            "SELECT v.node_id, v.embedding FROM claim_vectors v \
             JOIN propositions p ON p.node_id = v.node_id \
             WHERE v.embedding_model = ? AND p.status = 'active' ORDER BY v.node_id",
        )
        .bind(model)
        .fetch_all(self.read_pool())
        .await?;
        Ok(rows.into_iter().map(|(id, b)| (id, decode_embedding(&b))).collect())
    }

    /// The `k` active claims nearest `query` in `model`'s space by cosine, as
    /// `(node_id, cosine, vector)`, best first. Scored in SQL by sqlite-vec
    /// (`vec_distance_cosine` over the stored float32 blobs), so only the
    /// `k` winners are decoded; a vector of another width is skipped.
    pub async fn nearest_claims(
        &self,
        query: &[f32],
        model: &str,
        k: usize,
    ) -> Result<Vec<(String, f64, Vec<f32>)>> {
        if query.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let rows: Vec<(String, f64, Vec<u8>)> = sqlx::query_as(
            "SELECT node_id, dist, embedding FROM (
                SELECT v.node_id AS node_id,
                       vec_distance_cosine(v.embedding, ?) AS dist,
                       v.embedding AS embedding
                FROM claim_vectors v
                JOIN propositions p ON p.node_id = v.node_id
                WHERE v.embedding_model = ? AND p.status = 'active'
                  AND length(v.embedding) = ?
             )
             WHERE dist IS NOT NULL
             ORDER BY dist ASC, node_id ASC
             LIMIT ?",
        )
        .bind(encode_embedding(query))
        .bind(model)
        .bind((query.len() * 4) as i64)
        .bind(k as i64)
        .fetch_all(self.read_pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, dist, b)| (id, 1.0 - dist, decode_embedding(&b)))
            .collect())
    }

    /// Active claims with a vector in `model`'s space.
    pub async fn count_active_claim_vectors(&self, model: &str) -> Result<usize> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM claim_vectors v \
             JOIN propositions p ON p.node_id = v.node_id \
             WHERE v.embedding_model = ? AND p.status = 'active'",
        )
        .bind(model)
        .fetch_one(self.read_pool())
        .await?;
        Ok(n.max(0) as usize)
    }

    /// Mean of the active claim vectors in `model`'s space (at the most
    /// common width; a stray vector of another width is skipped), with how
    /// many were averaged.
    pub async fn claim_mean_vector_with_count(
        &self,
        model: &str,
    ) -> Result<Option<(Vec<f32>, usize)>> {
        let all = self.active_claim_vectors(model).await?;
        let mut widths: std::collections::HashMap<usize, usize> = Default::default();
        for (_, v) in &all {
            if !v.is_empty() {
                *widths.entry(v.len()).or_default() += 1;
            }
        }
        let Some(dim) = widths
            .into_iter()
            .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
            .map(|(d, _)| d)
        else {
            return Ok(None);
        };
        let mut sum = vec![0.0f64; dim];
        let mut n = 0usize;
        for (_, v) in &all {
            if v.len() != dim {
                continue;
            }
            for (i, &x) in v.iter().enumerate() {
                sum[i] += x as f64;
            }
            n += 1;
        }
        if n == 0 {
            return Ok(None);
        }
        Ok(Some((sum.iter().map(|s| (s / n as f64) as f32).collect(), n)))
    }

    /// Open a relation. Idempotent: re-opening keeps the first row (its
    /// strength and time).
    pub async fn insert_relation(&self, r: &Relation) -> Result<bool> {
        if r.from_node == r.to_node {
            return Ok(false);
        }
        Ok(sqlx::query(
            "INSERT OR IGNORE INTO proposition_relations \
             (from_node, to_node, kind, strength, reason, opened_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&r.from_node)
        .bind(&r.to_node)
        .bind(&r.kind)
        .bind(r.strength.clamp(0.0, 1.0))
        .bind(&r.reason)
        .bind(&r.opened_at)
        .execute(&*self.write().await)
        .await?
        .rows_affected()
            > 0)
    }

    /// Open relations with `node_id` at either end, oldest first.
    pub async fn open_relations_of(&self, node_id: &str) -> Result<Vec<Relation>> {
        let rows: Vec<RelationRow> = sqlx::query_as(
            "SELECT from_node, to_node, kind, strength, reason, opened_at FROM proposition_relations \
             WHERE closed_at IS NULL AND (from_node = ? OR to_node = ?) ORDER BY opened_at, from_node, to_node",
        )
        .bind(node_id)
        .bind(node_id)
        .fetch_all(self.read_pool())
        .await?;
        Ok(rows.into_iter().map(to_relation).collect())
    }

    /// The newer claim that supersedes `node_id`, if one does (open
    /// `supersedes` relation pointing at it).
    pub async fn superseded_by(&self, node_id: &str) -> Result<Option<Relation>> {
        let row: Option<RelationRow> = sqlx::query_as(
            "SELECT from_node, to_node, kind, strength, reason, opened_at FROM proposition_relations \
             WHERE closed_at IS NULL AND kind = 'supersedes' AND to_node = ? ORDER BY opened_at DESC LIMIT 1",
        )
        .bind(node_id)
        .fetch_optional(self.read_pool())
        .await?;
        Ok(row.map(to_relation))
    }

    /// Strongest open `disputes` relation touching `node_id` whose opposing
    /// proposition is still active and unsuperseded, if any.
    pub async fn max_open_claim_dispute(&self, node_id: &str) -> Result<Option<f64>> {
        Ok(sqlx::query_scalar(
            "SELECT MAX(r.strength)
             FROM proposition_relations AS r
             JOIN propositions AS opp
               ON opp.node_id = CASE WHEN r.from_node = ? THEN r.to_node ELSE r.from_node END
             WHERE r.closed_at IS NULL
               AND r.kind = 'disputes'
               AND (r.from_node = ? OR r.to_node = ?)
               AND opp.status = 'active'
               AND NOT EXISTS (
                   SELECT 1 FROM proposition_relations AS s
                   WHERE s.closed_at IS NULL AND s.kind = 'supersedes' AND s.to_node = opp.node_id
               )",
        )
        .bind(node_id)
        .bind(node_id)
        .bind(node_id)
        .fetch_one(self.read_pool())
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::propositions::Proposition;

    fn prop(id: &str) -> Proposition {
        Proposition {
            node_id: id.into(),
            claim: format!("claim {id}"),
            confidence: 0.5,
            is_disputed: false,
            status: "active".into(),
            importance: "medium".into(),
            urgency: "low".into(),
            urgency_expires_at: None,
            last_assessed_at: None,
            archived_at: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            attribution: String::new(),
            stance: "states".into(),
        }
    }

    #[tokio::test]
    async fn supersession_and_disputes_are_found_from_either_end() {
        let db = Db::open_in_memory().await.unwrap();
        for id in ["old", "new", "rival"] {
            db.insert_proposition(&prop(id)).await.unwrap();
        }
        let rel = |from: &str, to: &str, kind: &str, s: f64| Relation {
            from_node: from.into(),
            to_node: to.into(),
            kind: kind.into(),
            strength: s,
            reason: String::new(),
            opened_at: "2026-02-01T00:00:00Z".into(),
        };
        assert!(db.insert_relation(&rel("new", "old", "supersedes", 1.0)).await.unwrap());
        assert!(!db.insert_relation(&rel("new", "old", "supersedes", 0.3)).await.unwrap(), "idempotent");
        db.insert_relation(&rel("rival", "new", "disputes", 0.7)).await.unwrap();

        assert_eq!(db.superseded_by("old").await.unwrap().unwrap().from_node, "new");
        assert!(db.superseded_by("new").await.unwrap().is_none());
        assert_eq!(db.max_open_claim_dispute("new").await.unwrap(), Some(0.7));
        assert_eq!(db.max_open_claim_dispute("old").await.unwrap(), None);
        assert_eq!(db.open_relations_of("new").await.unwrap().len(), 2);

        // Superseding or archiving the rival clears the open dispute against `new`.
        db.insert_proposition(&prop("newer_rival")).await.unwrap();
        db.insert_relation(&rel("newer_rival", "rival", "supersedes", 1.0))
            .await
            .unwrap();
        assert_eq!(db.max_open_claim_dispute("new").await.unwrap(), None);

        db.insert_proposition(&prop("second_rival")).await.unwrap();
        db.insert_relation(&rel("second_rival", "new", "disputes", 0.9))
            .await
            .unwrap();
        assert_eq!(db.max_open_claim_dispute("new").await.unwrap(), Some(0.9));
        db.archive_proposition("second_rival", "2026-03-01T00:00:00Z")
            .await
            .unwrap();
        assert_eq!(db.max_open_claim_dispute("new").await.unwrap(), None);
    }

    #[tokio::test]
    async fn nearest_claims_ranks_in_sql_and_skips_other_widths() {
        let db = Db::open_in_memory().await.unwrap();
        for (id, v) in [
            ("near", vec![1.0f32, 0.1, 0.0]),
            ("far", vec![0.0f32, 0.0, 1.0]),
            ("mid", vec![0.7f32, 0.7, 0.0]),
        ] {
            db.insert_proposition(&prop(id)).await.unwrap();
            db.upsert_claim_vector(id, "m", &v).await.unwrap();
        }
        db.insert_proposition(&prop("wide")).await.unwrap();
        db.upsert_claim_vector("wide", "m", &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        let hits = db.nearest_claims(&[1.0, 0.0, 0.0], "m", 2).await.unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.0.as_str()).collect();
        assert_eq!(ids, ["near", "mid"]);
        assert!(hits[0].1 > 0.99 && hits[0].2.len() == 3);
        assert_eq!(db.count_active_claim_vectors("m").await.unwrap(), 4);
        let (mean, n) = db.claim_mean_vector_with_count("m").await.unwrap().unwrap();
        assert_eq!(n, 3, "the odd-width vector is left out of the mean");
        assert_eq!(mean.len(), 3);
    }
}
