//! Regression coverage for issue #4: `forget_by_text`'s semantic fallback
//! must match on the embedding the *caller* supplies (the engine's real
//! embedder), not one it silently re-derives internally via the lexical
//! hashing fallback -- which would compare against a different, unrelated
//! vector space from whatever embedded the stored belief.

use adaptive_pathway::store::beliefs::{Belief, Layer, Provenance};
use adaptive_pathway::store::suppressions::SuppressReason;
use adaptive_pathway::store::Db;
use chrono::Utc;

async fn seed_belief(db: &Db, id: &str, embedding: Vec<f32>) {
    let now = Utc::now();
    db.insert_belief(&Belief {
        id: id.into(),
        text: "The user prefers dense, information-heavy responses.".into(),
        embedding,
        confidence: 0.7,
        provenance: Provenance::DirectStatement,
        layer: Layer::Context,
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
}

#[tokio::test]
async fn cosine_fallback_matches_on_the_supplied_embedding_not_a_rederived_one() {
    let db = Db::open_in_memory().await.unwrap();
    // A synthetic "real embedder" vector for the belief -- the exact bytes
    // don't matter, only that `forget_by_text` is given the *same* vector
    // space to compare against, not one it invents internally.
    let belief_vec = vec![0.2_f32, 0.9, -0.1, 0.05];
    seed_belief(&db, "b1", belief_vec.clone()).await;

    // A paraphrase that does not textually match the belief at all, so this
    // can only resolve through the cosine fallback -- and only if the
    // caller-supplied embedding is actually the one compared.
    let paraphrase = "I want thorough, detailed answers please";
    let dropped = db
        .forget_by_text(paraphrase, &belief_vec, &[], SuppressReason::Wrong)
        .await
        .unwrap();
    assert_eq!(
        dropped.as_deref(),
        Some("The user prefers dense, information-heavy responses."),
        "must resolve via the supplied embedding's cosine similarity"
    );
}

#[tokio::test]
async fn cosine_fallback_does_not_match_an_unrelated_embedding() {
    let db = Db::open_in_memory().await.unwrap();
    seed_belief(&db, "b1", vec![1.0, 0.0, 0.0, 0.0]).await;

    // A query embedding orthogonal to the belief's -- must not resolve, even
    // though the text similarly doesn't match. Proves the match is actually
    // driven by the passed vector rather than some other implicit lookup.
    let orthogonal = vec![0.0_f32, 1.0, 0.0, 0.0];
    let dropped = db
        .forget_by_text("totally unrelated phrase", &orthogonal, &[], SuppressReason::Wrong)
        .await
        .unwrap();
    assert_eq!(dropped, None);
}

#[tokio::test]
async fn empty_embedding_skips_the_cosine_fallback_entirely() {
    let db = Db::open_in_memory().await.unwrap();
    seed_belief(&db, "b1", vec![1.0, 0.0, 0.0, 0.0]).await;

    // An empty embedding (e.g. a caller with no embedder handy) must not
    // spuriously match anything via a degenerate zero-vector comparison.
    let dropped = db
        .forget_by_text("something", &[], &[], SuppressReason::Wrong)
        .await
        .unwrap();
    assert_eq!(dropped, None);
}
