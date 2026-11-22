//! Staged cross-session behavioral evidence and post-turn signal feedback
//! telemetry (migration 009).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{decode_embedding, encode_embedding, Db};
use crate::error::Result;
use crate::store::beliefs::{BeliefKind, Provenance};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagedEvidence {
    pub id: String,
    pub session_id: String,
    pub statement: String,
    pub kind: BeliefKind,
    pub provenance: Provenance,
    pub domain: Option<String>,
    pub evidence: Option<String>,
    pub embedding: Vec<f32>,
    pub embedding_model: String,
    pub created_at: DateTime<Utc>,
    pub synthesized_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct StagedRow {
    id: String,
    session_id: String,
    statement: String,
    kind: String,
    provenance: String,
    domain: Option<String>,
    evidence: Option<String>,
    embedding: Vec<u8>,
    embedding_model: String,
    created_at: DateTime<Utc>,
    synthesized_at: Option<DateTime<Utc>>,
}

impl From<StagedRow> for StagedEvidence {
    fn from(r: StagedRow) -> Self {
        Self {
            id: r.id,
            session_id: r.session_id,
            statement: r.statement,
            kind: BeliefKind::parse(&r.kind),
            provenance: Provenance::parse(&r.provenance),
            domain: r.domain,
            evidence: r.evidence,
            embedding: decode_embedding(&r.embedding),
            embedding_model: r.embedding_model,
            created_at: r.created_at,
            synthesized_at: r.synthesized_at,
        }
    }
}

impl Db {
    pub async fn insert_staged_evidence(&self, item: &StagedEvidence) -> Result<()> {
        sqlx::query(
            "INSERT OR IGNORE INTO staged_evidence (
                id, session_id, statement, kind, provenance, domain, evidence,
                embedding, embedding_model, created_at, synthesized_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&item.id)
        .bind(&item.session_id)
        .bind(&item.statement)
        .bind(item.kind.as_str())
        .bind(item.provenance.as_str())
        .bind(&item.domain)
        .bind(&item.evidence)
        .bind(encode_embedding(&item.embedding))
        .bind(&item.embedding_model)
        .bind(item.created_at)
        .bind(item.synthesized_at)
        .execute(&*self.write().await)
        .await?;
        Ok(())
    }

    pub async fn list_pending_staged_evidence(&self, limit: usize) -> Result<Vec<StagedEvidence>> {
        let rows: Vec<StagedRow> = sqlx::query_as(
            "SELECT id, session_id, statement, kind, provenance, domain, evidence,
                    embedding, embedding_model, created_at, synthesized_at
             FROM staged_evidence
             WHERE synthesized_at IS NULL
             ORDER BY created_at ASC, id ASC
             LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(self.read_pool())
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn mark_staged_synthesized(
        &self,
        ids: &[String],
        now: DateTime<Utc>,
    ) -> Result<usize> {
        let mut updated = 0usize;
        for id in ids {
            let res = sqlx::query(
                "UPDATE staged_evidence SET synthesized_at = ? WHERE id = ? AND synthesized_at IS NULL",
            )
            .bind(now)
            .bind(id)
            .execute(&*self.write().await)
            .await?;
            updated += res.rows_affected() as usize;
        }
        Ok(updated)
    }

    /// The most similar pending staged item from a *different* session in the
    /// same embedding space, created at or after `since`, whose cosine with
    /// `embedding` clears `min_cosine`. This is the model-free corroboration
    /// path: the same kind of observation turning up in a second chat.
    pub async fn find_corroborating_staged(
        &self,
        embedding: &[f32],
        embedding_model: &str,
        exclude_session: &str,
        since: DateTime<Utc>,
        min_cosine: f64,
    ) -> Result<Option<(StagedEvidence, f64)>> {
        if embedding.is_empty() {
            return Ok(None);
        }
        let rows: Vec<StagedRow> = sqlx::query_as(
            "SELECT id, session_id, statement, kind, provenance, domain, evidence,
                    embedding, embedding_model, created_at, synthesized_at
             FROM staged_evidence
             WHERE synthesized_at IS NULL AND embedding_model = ?
               AND session_id != ? AND created_at >= ?
             ORDER BY created_at DESC, id ASC
             LIMIT 500",
        )
        .bind(embedding_model)
        .bind(exclude_session)
        .bind(since)
        .fetch_all(self.read_pool())
        .await?;
        let mut best: Option<(StagedEvidence, f64)> = None;
        for row in rows {
            let item: StagedEvidence = row.into();
            if item.embedding.len() != embedding.len() {
                continue;
            }
            let cos = crate::vector::ops::cosine(&item.embedding, embedding);
            if cos >= min_cosine && best.as_ref().is_none_or(|(_, b)| cos > *b) {
                best = Some((item, cos));
            }
        }
        Ok(best)
    }

    /// Newest `created_at` among pending staged items, so synthesis can
    /// skip a pending set it has already judged.
    pub async fn newest_pending_staged_at(&self) -> Result<Option<DateTime<Utc>>> {
        Ok(sqlx::query_scalar(
            "SELECT MAX(created_at) FROM staged_evidence WHERE synthesized_at IS NULL",
        )
        .fetch_one(self.read_pool())
        .await?)
    }

    pub async fn expire_old_staged_evidence(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        let res = sqlx::query(
            "DELETE FROM staged_evidence WHERE synthesized_at IS NULL AND created_at < ?",
        )
        .bind(cutoff)
        .execute(&*self.write().await)
        .await?;
        Ok(res.rows_affected() as usize)
    }

    pub async fn record_signal_feedback(
        &self,
        session_id: &str,
        signal_kind: &str,
        engaged: bool,
        now: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO signal_feedback (session_id, signal_kind, engaged, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(session_id)
        .bind(signal_kind)
        .bind(if engaged { 1i64 } else { 0i64 })
        .bind(now)
        .execute(&*self.write().await)
        .await?;
        Ok(())
    }

    pub async fn signal_engagement_rate(
        &self,
        signal_kind: &str,
        last_n: usize,
    ) -> Result<Option<(usize, usize)>> {
        if last_n == 0 {
            return Ok(None);
        }
        let rows: Vec<i64> = sqlx::query_scalar(
            "SELECT engaged FROM signal_feedback
             WHERE signal_kind = ?
             ORDER BY id DESC
             LIMIT ?",
        )
        .bind(signal_kind)
        .bind(last_n as i64)
        .fetch_all(self.read_pool())
        .await?;
        if rows.is_empty() {
            return Ok(None);
        }
        let engaged = rows.iter().filter(|&&v| v != 0).count();
        Ok(Some((engaged, rows.len())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[tokio::test]
    async fn staged_evidence_roundtrip_synthesis_and_expiry() {
        let db = Db::open_in_memory().await.unwrap();
        let now = Utc::now();
        let old = now - Duration::days(20);
        db.insert_staged_evidence(&StagedEvidence {
            id: "st_old".into(),
            session_id: "s1".into(),
            statement: "Old uncorroborated observation".into(),
            kind: BeliefKind::Preference,
            provenance: Provenance::InferredPattern,
            domain: None,
            evidence: None,
            embedding: vec![1.0, 0.0],
            embedding_model: "m".into(),
            created_at: old,
            synthesized_at: None,
        })
        .await
        .unwrap();
        db.insert_staged_evidence(&StagedEvidence {
            id: "st_new".into(),
            session_id: "s2".into(),
            statement: "Relies on ArXiv preprints".into(),
            kind: BeliefKind::Preference,
            provenance: Provenance::InferredPattern,
            domain: Some("research".into()),
            evidence: Some("reads arxiv".into()),
            embedding: vec![0.0, 1.0],
            embedding_model: "m".into(),
            created_at: now,
            synthesized_at: None,
        })
        .await
        .unwrap();

        let expired = db
            .expire_old_staged_evidence(now - Duration::days(14))
            .await
            .unwrap();
        assert_eq!(expired, 1);
        let pending = db.list_pending_staged_evidence(10).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "st_new");

        assert_eq!(
            db.mark_staged_synthesized(&["st_new".into()], now)
                .await
                .unwrap(),
            1
        );
        assert!(db.list_pending_staged_evidence(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn signal_feedback_tracks_recent_engagement_rate() {
        let db = Db::open_in_memory().await.unwrap();
        let now = Utc::now();
        assert_eq!(db.signal_engagement_rate("challenge", 10).await.unwrap(), None);
        db.record_signal_feedback("s1", "challenge", true, now).await.unwrap();
        db.record_signal_feedback("s2", "challenge", false, now).await.unwrap();
        db.record_signal_feedback("s3", "challenge", true, now).await.unwrap();
        assert_eq!(
            db.signal_engagement_rate("challenge", 2).await.unwrap(),
            Some((1, 2))
        );
        assert_eq!(
            db.signal_engagement_rate("challenge", 10).await.unwrap(),
            Some((2, 3))
        );
    }
}
