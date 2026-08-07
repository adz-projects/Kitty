//! Consolidation: promote conversation/context beliefs up the layer stack and
//! merge redundant ones, gated by the plan's four promotion gates.

use chrono::Utc;

use crate::error::Result;
use crate::store::beliefs::{Belief, Layer, Provenance, BeliefPatch};
use crate::store::Db;

/// Cosine for merging beliefs within the same layer during consolidation.
const CONSOLIDATE_MERGE_COSINE: f64 = 0.86;

/// Promote a context belief to identity only when ALL four gates hold:
/// support_count ≥ 3 AND distinct_sessions ≥ 2 AND
/// (provenance ∈ {direct_statement, controlled_test, correction} OR tested)
/// AND confidence ≥ 0.65. The two-session gate stops one chatty conversation
/// writing a permanent fact.
pub fn promotion_gates_pass(b: &Belief) -> bool {
    b.support_count >= 3
        && b.distinct_sessions >= 2
        && (matches!(
            b.provenance,
            Provenance::DirectStatement | Provenance::ControlledTest | Provenance::Correction
        ) || b.tested)
        && b.confidence >= 0.65
}

/// Merge conversation-layer beliefs into context at cosine ≥ 0.86, then
/// promote context → identity where the gates hold, then run a contradiction
/// pass.
pub async fn consolidate_session(db: &Db, session_id: &str) -> Result<()> {
    let now = Utc::now();

    // Load conversation-layer beliefs, discard weak ones.
    let conversation = db.list_beliefs(Some(Layer::Conversation)).await?;
    for b in conversation {
        // discard confidence < 0.25 or single-observation-with-support-1
        // (keep observations as audit trail -- they're preserved by the
        // observations table)
        let weak = b.confidence < 0.25 || (b.provenance == Provenance::SingleObservation && b.support_count <= 1);
        if weak {
            db.delete_belief(&b.id).await?;
            continue;
        }
        // merge into a context belief at cosine ≥ 0.86
        let context = db.list_beliefs(Some(Layer::Context)).await?;
        let mut merged = false;
        for c in context.iter() {
            let cos = crate::vector::ops::cosine(&c.embedding, &b.embedding);
            if cos >= CONSOLIDATE_MERGE_COSINE {
                db.update_belief(
                    &c.id,
                    &BeliefPatch {
                        support_count: Some(c.support_count + b.support_count),
                        distinct_sessions: Some(c.distinct_sessions.max(b.distinct_sessions)),
                        confidence: Some(c.confidence.max(b.confidence)),
                        ..Default::default()
                    },
                    now,
                )
                .await?;
                // re-parent observations
                sqlx::query("UPDATE observations SET belief_id = ? WHERE belief_id = ?")
                    .bind(&c.id)
                    .bind(&b.id)
                    .execute(db.pool())
                    .await?;
                db.delete_belief(&b.id).await?;
                merged = true;
                break;
            }
        }
        if !merged {
            // promote conversation -> context (it's durable enough to leave
            // the conversation layer, just not identity yet)
            db.update_belief(
                &b.id,
                &BeliefPatch {
                    layer: Some(Layer::Context),
                    ..Default::default()
                },
                now,
            )
            .await?;
        }
    }

    // Promote context -> identity where gates pass.
    let context = db.list_beliefs(Some(Layer::Context)).await?;
    for b in context {
        if promotion_gates_pass(&b) {
            db.update_belief(
                &b.id,
                &BeliefPatch {
                    layer: Some(Layer::Identity),
                    consolidated_at: Some(now),
                    ..Default::default()
                },
                now,
            )
            .await?;
        }
    }

    // Contradiction pass over open contradictions.
    crate::store::contradictions::run_contradiction_pass(db).await?;

    // Audit.
    db.audit("consolidate", Some(&format!("session={session_id}"))).await?;
    Ok(())
}
