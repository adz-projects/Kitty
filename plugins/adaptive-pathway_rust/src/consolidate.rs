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
/// pass. Scoped to `session_id`'s own conversation-layer beliefs only --
/// conversation beliefs are session-owned ("lives for the session"), so
/// consolidating one session must never touch, delete, or promote another
/// session's still-active conversational memory.
///
/// Runs inside one `BEGIN`/`COMMIT` transaction (`Db::run_in_transaction`):
/// a crash or error partway through used to leave some beliefs merged/
/// promoted and others not, with no way to tell which pass a given belief's
/// state came from.
pub async fn consolidate_session(db: &Db, session_id: &str) -> Result<()> {
    db.run_in_transaction(move || consolidate_session_inner(db, session_id)).await
}

async fn consolidate_session_inner(db: &Db, session_id: &str) -> Result<()> {
    let now = Utc::now();

    // Load *this session's* conversation-layer beliefs only, discard weak
    // ones.
    let conversation = db.list_conversation_beliefs_for_session(session_id).await?;

    // Context beliefs are cross-session by design, so this search is
    // intentionally NOT session-scoped. Loaded *once* here, then kept in
    // sync locally as this pass merges/promotes into it -- re-fetching the
    // full context-layer table from the DB inside the conversation loop
    // (the previous shape) made one consolidation pass O(conversation_count
    // × context_count) DB reads instead of O(context_count). Correctness of
    // keeping this local copy in sync matters: a belief's *embedding* never
    // changes on merge (only confidence/support/etc. do), so cosine search
    // against a snapshot is always valid; the fields updated on merge are
    // updated in the local copy too so a second conversation belief in the
    // same pass sees the first merge's effect rather than clobbering it.
    let mut context = db.list_beliefs(Some(Layer::Context)).await?;

    for b in conversation {
        // discard confidence < 0.25 or single-observation-with-support-1
        // (keep observations as audit trail -- they're preserved by the
        // observations table)
        let weak = b.confidence < 0.25 || (b.provenance == Provenance::SingleObservation && b.support_count <= 1);
        if weak {
            db.delete_belief(&b.id).await?;
            continue;
        }
        // merge into a context belief at cosine ≥ 0.86.
        let mut merged = false;
        for c in context.iter_mut() {
            let cos = crate::vector::ops::cosine(&c.embedding, &b.embedding);
            if cos >= CONSOLIDATE_MERGE_COSINE {
                // Reinforcement, not a blind max(): repeated supporting
                // evidence should actually strengthen a belief (the plan's
                // multiplicative-toward-the-bound step), and merging in
                // evidence gathered under a *different* session is exactly
                // what distinct_sessions exists to count -- `.max()` on two
                // rows that both start at 1 can never clear the promotion
                // gate's `distinct_sessions >= 2` requirement.
                let new_conf = crate::belief::synthesis::merge_confidence(c.confidence, b.provenance);
                let new_prov = crate::belief::synthesis::max_provenance(c.provenance, b.provenance);
                // b came from THIS session (we loaded it via
                // list_conversation_beliefs_for_session), so merging it in
                // always contributes evidence from `session_id`; only count
                // it as a *new* distinct session for `c` if `c` wasn't
                // already attributed to this session.
                let session_is_new = c.session_id.as_deref() != Some(session_id);
                let new_support = c.support_count + b.support_count;
                let new_sessions = if session_is_new { c.distinct_sessions + 1 } else { c.distinct_sessions };
                let new_tested = c.tested || new_prov.is_tested();
                db.update_belief(
                    &c.id,
                    &BeliefPatch {
                        confidence: Some(new_conf),
                        support_count: Some(new_support),
                        distinct_sessions: Some(new_sessions),
                        // Carried onto the belief itself, not just the
                        // observation row -- see the matching comment in
                        // `synthesis::route_observation`.
                        provenance: Some(new_prov),
                        tested: Some(new_tested),
                        last_confirmed_at: Some(now),
                        ..Default::default()
                    },
                    now,
                )
                .await?;
                // Keep the local snapshot in sync so a later conversation
                // belief in this same pass merging into the same `c` builds
                // on this update instead of overwriting it with stale data.
                c.confidence = new_conf;
                c.support_count = new_support;
                c.distinct_sessions = new_sessions;
                c.provenance = new_prov;
                c.tested = new_tested;
                c.last_confirmed_at = Some(now);
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
            // the conversation layer, just not identity yet). Context is
            // cross-session, so this also clears session_id.
            db.update_belief(
                &b.id,
                &BeliefPatch {
                    layer: Some(Layer::Context),
                    session_id: Some(None),
                    ..Default::default()
                },
                now,
            )
            .await?;
            // Append to the local snapshot too -- a later conversation
            // belief in this same pass must be able to merge into a belief
            // that only just got promoted, exactly as it could when
            // `context` was re-fetched fresh from the DB every iteration.
            let mut promoted = b.clone();
            promoted.layer = Layer::Context;
            promoted.session_id = None;
            context.push(promoted);
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
