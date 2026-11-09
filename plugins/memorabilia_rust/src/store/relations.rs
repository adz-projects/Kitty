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
        .execute(self.pool())
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
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(|(id, b)| (id, decode_embedding(&b))).collect())
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
        .execute(self.pool())
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
        .fetch_all(self.pool())
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
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(to_relation))
    }

    /// Strongest open `disputes` relation touching `node_id`, if any.
    pub async fn max_open_claim_dispute(&self, node_id: &str) -> Result<Option<f64>> {
        Ok(sqlx::query_scalar(
            "SELECT MAX(strength) FROM proposition_relations \
             WHERE closed_at IS NULL AND kind = 'disputes' AND (from_node = ? OR to_node = ?)",
        )
        .bind(node_id)
        .bind(node_id)
        .fetch_one(self.pool())
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
    }
}
