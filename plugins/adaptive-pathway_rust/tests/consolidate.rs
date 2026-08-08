//! Regression coverage for issue #2: `consolidate_session` must only ever
//! touch the given session's own conversation-layer beliefs, never another
//! session's still-fast-decaying conversational memory.

use adaptive_pathway::consolidate::consolidate_session;
use adaptive_pathway::store::beliefs::{Belief, Layer, Provenance};
use adaptive_pathway::store::Db;
use chrono::Utc;

fn conversation_belief(id: &str, session_id: &str, confidence: f64, support: i64) -> Belief {
    let now = Utc::now();
    Belief {
        id: id.into(),
        text: format!("belief {id}"),
        embedding: vec![1.0, 0.0],
        confidence,
        provenance: Provenance::DirectStatement,
        layer: Layer::Conversation,
        tested: true,
        domain: None,
        tier: "conversation".into(),
        support_count: support,
        distinct_sessions: 1,
        contradict_count: 0,
        pinned: false,
        last_confirmed_at: Some(now),
        consolidated_at: None,
        created_at: now,
        updated_at: now,
        session_id: Some(session_id.into()),
    }
}

#[tokio::test]
async fn consolidating_one_session_never_touches_another_sessions_conversation_beliefs() {
    let db = Db::open_in_memory().await.unwrap();

    // A weak session-A belief that should be discarded by consolidation...
    let weak_a = conversation_belief("weak-a", "session-a", 0.10, 1);
    // ...and a strong, promotion-eligible session-B belief that must survive
    // completely untouched by consolidating session A.
    let mut strong_b = conversation_belief("strong-b", "session-b", 0.90, 5);
    strong_b.distinct_sessions = 3;

    db.insert_belief(&weak_a).await.unwrap();
    db.insert_belief(&strong_b).await.unwrap();

    consolidate_session(&db, "session-a").await.unwrap();

    // Session A's weak belief is gone.
    assert!(db.get_belief("weak-a").await.unwrap().is_none());

    // Session B's belief is completely untouched: still conversation-layer,
    // still owned by session-b, confidence/support unchanged.
    let untouched = db.get_belief("strong-b").await.unwrap().unwrap();
    assert_eq!(untouched.layer, Layer::Conversation);
    assert_eq!(untouched.session_id.as_deref(), Some("session-b"));
    assert!((untouched.confidence - 0.90).abs() < 1e-9);
    assert_eq!(untouched.support_count, 5);
}

#[tokio::test]
async fn promotion_out_of_conversation_layer_clears_session_id() {
    let db = Db::open_in_memory().await.unwrap();
    // Promotion out of the conversation layer (merge-or-promote) is
    // unconditional for any non-weak belief -- but the *identity* gate
    // additionally requires distinct_sessions >= 2, so distinct_sessions=1
    // here means it must land at Context and stop, not jump straight to
    // Identity in the same pass.
    let mut b = conversation_belief("promote-me", "session-a", 0.70, 3);
    b.distinct_sessions = 1;
    db.insert_belief(&b).await.unwrap();

    consolidate_session(&db, "session-a").await.unwrap();

    let promoted = db.get_belief("promote-me").await.unwrap().unwrap();
    assert_eq!(promoted.layer, Layer::Context, "durable belief should leave the conversation layer");
    assert_eq!(promoted.session_id, None, "context beliefs are cross-session and must not carry a session_id");
}

#[tokio::test]
async fn distinct_sessions_increments_on_genuinely_new_session_merge() {
    let db = Db::open_in_memory().await.unwrap();

    // Session A's belief promotes to context first. distinct_sessions=1
    // deliberately keeps it below the identity gate's >=2 requirement, so it
    // stops at Context and remains a valid merge target for session B.
    let mut a = conversation_belief("from-a", "session-a", 0.70, 3);
    a.distinct_sessions = 1;
    db.insert_belief(&a).await.unwrap();
    consolidate_session(&db, "session-a").await.unwrap();
    let context_belief = db.get_belief("from-a").await.unwrap().unwrap();
    assert_eq!(context_belief.layer, Layer::Context);
    let sessions_after_a = context_belief.distinct_sessions;

    // A near-identical belief from session B should merge into it and bump
    // distinct_sessions (a genuinely new session), not just max() the counts.
    let b = conversation_belief("from-b", "session-b", 0.60, 1);
    db.insert_belief(&b).await.unwrap();
    consolidate_session(&db, "session-b").await.unwrap();

    let merged = db.get_belief("from-a").await.unwrap().unwrap();
    assert!(
        merged.distinct_sessions > sessions_after_a,
        "merging in evidence from a new session must increment distinct_sessions (was {sessions_after_a}, now {})",
        merged.distinct_sessions
    );
    assert!(db.get_belief("from-b").await.unwrap().is_none(), "the merged-away belief should be deleted");
}

/// Regression guard for issue #26's fix: `consolidate_session` used to
/// re-fetch the entire context-layer table from the DB on every
/// conversation-belief iteration (O(conversation_count × context_count)
/// reads) -- correct, since each fresh read saw every prior iteration's
/// already-committed write within the same transaction, but wasteful.
/// Hoisting that load to once per pass is only safe because the local copy
/// is kept in sync after each merge; *without* that sync-back, this exact
/// scenario -- two conversation beliefs from one pass merging into the same
/// context belief -- would silently drop the first merge's contribution
/// when the second one computed its reinforcement off the stale
/// pre-first-merge snapshot instead. This test pins that behavior so a
/// future "simplification" that drops the sync-back can't reintroduce it.
#[tokio::test]
async fn two_conversation_beliefs_merging_into_the_same_context_belief_both_apply() {
    let db = Db::open_in_memory().await.unwrap();
    let now = Utc::now();

    // A context belief with support_count=1 that both conversation beliefs
    // below are similar enough (identical embedding) to merge into.
    db.insert_belief(&Belief {
        id: "ctx".into(),
        text: "the user likes X".into(),
        embedding: vec![1.0, 0.0],
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
        last_confirmed_at: Some(now),
        consolidated_at: None,
        created_at: now,
        updated_at: now,
        session_id: None,
    })
    .await
    .unwrap();

    // Two conversation-layer beliefs from the SAME session, both matching
    // the context belief's embedding closely enough to merge (cosine 1.0).
    db.insert_belief(&conversation_belief("conv-1", "session-a", 0.7, 2)).await.unwrap();
    db.insert_belief(&conversation_belief("conv-2", "session-a", 0.7, 3)).await.unwrap();

    consolidate_session(&db, "session-a").await.unwrap();

    let merged = db.get_belief("ctx").await.unwrap().unwrap();
    // If the second merge used a stale pre-first-merge snapshot, support_count
    // would be 1 (base) + 3 (conv-2 only) = 4, silently dropping conv-1's
    // contribution. Correct behavior accumulates both: 1 + 2 + 3 = 6.
    assert_eq!(
        merged.support_count, 6,
        "both merges must accumulate onto support_count, not just the last one processed"
    );
    assert!(db.get_belief("conv-1").await.unwrap().is_none());
    assert!(db.get_belief("conv-2").await.unwrap().is_none());
}
