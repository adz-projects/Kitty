//! Consolidation: promote conversation/context beliefs up the layer stack and
//! merge redundant ones, gated by the plan's four promotion gates.

use chrono::Utc;

use crate::error::Result;
use crate::store::beliefs::{Belief, Layer, Provenance, BeliefPatch};
use crate::store::Db;

/// Cosine for merging beliefs within the same layer during consolidation.
const CONSOLIDATE_MERGE_COSINE: f64 = 0.86;

/// Cosine at which a staged one-off from another chat counts as the same
/// observation turning up again (looser than a merge: two chats rarely word
/// the same thing identically).
pub const STAGED_CORROBORATION_COSINE: f64 = 0.80;

/// Days a staged one-off waits for corroboration before it expires.
pub const STAGED_RETENTION_DAYS: i64 = 30;

/// Conversation beliefs below this are dropped rather than staged.
const DISCARD_BELOW_CONFIDENCE: f64 = 0.10;

/// Confidence at which a single-chat belief is trusted on its own.
const SELF_EVIDENT_CONFIDENCE: f64 = 0.60;

/// Whether a conversation belief may move into the context layer on its own
/// evidence: stated, corrected or tested; seen at least twice in the chat;
/// linked to something already known; already corroborated across chats; or
/// confident enough. Anything else is a one-off and is staged instead.
pub fn qualifies_for_context(b: &Belief, linked: bool) -> bool {
    matches!(
        b.provenance,
        Provenance::DirectStatement | Provenance::ControlledTest | Provenance::Correction
    ) || b.tested
        || b.support_count >= 2
        || b.distinct_sessions >= 2
        || linked
        || b.confidence >= SELF_EVIDENT_CONFIDENCE
}

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
/// promote context → identity where the gates hold. Scoped to `session_id`'s own conversation-layer beliefs only --
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
    // Beliefs this pass deleted, merged or moved. The audit row is written
    // only when something changed: the idle sweep revisits sessions, and an
    // unconditional row per visit filled `audit_log` at 3 rows a minute.
    let mut changed = 0usize;

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
    // Identity beliefs are merge targets too: a promoted fact must keep
    // receiving the evidence a conversation gathers for it.
    let mut context = db.list_beliefs(Some(Layer::Context)).await?;
    context.extend(db.list_beliefs(Some(Layer::Identity)).await?);

    for b in conversation {
        // Discard only what is close to worthless (contradicted down to
        // almost nothing). One-off observations are staged below instead, so
        // a second chat showing the same thing can still corroborate them.
        // Observations stay as the audit trail either way.
        if b.confidence < DISCARD_BELOW_CONFIDENCE {
            db.delete_belief(&b.id).await?;
            changed += 1;
            continue;
        }
        // merge into a context belief at cosine ≥ 0.86.
        let mut merged = false;
        for c in context.iter_mut() {
            if c.embedding_model != b.embedding_model {
                continue;
            }
            let cos = crate::vector::ops::cosine(&c.embedding, &b.embedding);
            if cos >= CONSOLIDATE_MERGE_COSINE {
                // Reinforcement, not a blind max(): repeated supporting
                // evidence should actually strengthen a belief (the plan's
                // multiplicative-toward-the-bound step), and merging in
                // evidence gathered under a *different* session is exactly
                // what distinct_sessions exists to count -- `.max()` on two
                // rows that both start at 1 can never clear the promotion
                // gate's `distinct_sessions >= 2` requirement.
                let new_prov = crate::belief::synthesis::max_provenance(c.provenance, b.provenance);
                let new_conf = crate::belief::synthesis::merge_confidence(
                    c.confidence,
                    b.provenance,
                    c.full_weight() || b.full_weight() || new_prov.is_tested() || new_prov == Provenance::DirectStatement,
                );
                // b came from THIS session (we loaded it via
                // list_conversation_beliefs_for_session), and its observations
                // are about to be re-parented onto `c`. Session-identity is
                // checked via the observations table — the old
                // `c.session_id != Some(session_id)` delta read a column
                // that's always `None` on cross-session Context targets, so a
                // single chatty session could keep inflating distinct_sessions
                // past the `>= 2` promotion gate on its own. Bump the
                // monotonic stored counter only when THIS session isn't
                // already represented among c's observations.
                let session_already_contributed =
                    db.has_session_observation(&c.id, session_id).await?;
                let distinct_sessions = c.distinct_sessions
                    + if session_already_contributed { 0 } else { 1 };
                let new_support = c.support_count + b.support_count;
                let new_tested = c.tested || b.tested || new_prov.is_tested();
                db.update_belief(
                    &c.id,
                    &BeliefPatch {
                        confidence: Some(new_conf),
                        support_count: Some(new_support),
                        distinct_sessions: Some(distinct_sessions),
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
                c.distinct_sessions = distinct_sessions;
                c.provenance = new_prov;
                c.tested = new_tested;
                c.last_confirmed_at = Some(now);
                // re-parent observations
                sqlx::query("UPDATE observations SET belief_id = ? WHERE belief_id = ?")
                    .bind(&c.id)
                    .bind(&b.id)
                    .execute(&*db.write().await)
                    .await?;
                db.delete_belief(&b.id).await?;
                merged = true;
                changed += 1;
                break;
            }
        }
        if !merged {
            let linked = db.has_any_link(&b.id).await?;
            // A one-off that another chat has already staged something very
            // like: corroborated across sessions, so it qualifies too.
            let corroboration = if qualifies_for_context(&b, linked) {
                None
            } else {
                db.find_corroborating_staged(
                    &b.embedding,
                    &b.embedding_model,
                    session_id,
                    now - chrono::Duration::days(STAGED_RETENTION_DAYS),
                    STAGED_CORROBORATION_COSINE,
                )
                .await?
            };
            if qualifies_for_context(&b, linked) || corroboration.is_some() {
                // Promote conversation -> context. Context is cross-session,
                // so this also clears session_id.
                let mut patch = BeliefPatch {
                    layer: Some(Layer::Context),
                    session_id: Some(None),
                    ..Default::default()
                };
                let mut promoted = b.clone();
                if let Some((staged, _)) = &corroboration {
                    let conf = crate::belief::synthesis::merge_confidence(
                        b.confidence,
                        staged.provenance,
                        false,
                    );
                    let sessions = b.distinct_sessions.max(1) + 1;
                    let support = b.support_count + 1;
                    patch.confidence = Some(conf);
                    patch.distinct_sessions = Some(sessions);
                    patch.support_count = Some(support);
                    patch.last_confirmed_at = Some(now);
                    promoted.confidence = conf;
                    promoted.distinct_sessions = sessions;
                    promoted.support_count = support;
                    promoted.last_confirmed_at = Some(now);
                    db.mark_staged_synthesized(std::slice::from_ref(&staged.id), now)
                        .await?;
                }
                db.update_belief(&b.id, &patch, now).await?;
                promoted.layer = Layer::Context;
                promoted.session_id = None;
                context.push(promoted);
                changed += 1;
            } else {
                // Uncorroborated one-off: staged, not promoted. It becomes a
                // belief if a later chat shows something similar (above) or
                // cross-session synthesis finds a pattern in it; otherwise it
                // expires after `STAGED_RETENTION_DAYS`. Deleting the
                // conversation belief sets `observations.belief_id = NULL`
                // (FK ON DELETE SET NULL), keeping the raw audit trail.
                db.insert_staged_evidence(&crate::store::staged::StagedEvidence {
                    id: crate::store::audit::uuid_string(),
                    session_id: session_id.to_string(),
                    statement: b.text.clone(),
                    kind: b.kind,
                    provenance: b.provenance,
                    domain: b.domain.clone(),
                    evidence: None,
                    embedding: b.embedding.clone(),
                    embedding_model: b.embedding_model.clone(),
                    created_at: now,
                    synthesized_at: None,
                })
                .await?;
                db.delete_belief(&b.id).await?;
                changed += 1;
            }
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
            changed += 1;
        }
    }

    if changed > 0 {
        db.audit("consolidate", Some(&format!("session={session_id} changed={changed}")))
            .await?;
    }
    Ok(())
}
