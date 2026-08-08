//! End-to-end coverage for the embedding-model-change migration
//! (`migrations/005_belief_embedding_model.sql`, `PathwayEngine::open`'s
//! fingerprint check, `store::beliefs::list_recall_candidates`'s filter,
//! `background::reembed_stale_beliefs`): switching the configured embedding
//! model must never let a stale-space belief silently compare against a
//! fresh-space query via cosine, and the background re-embed pass must
//! restore eligibility without disturbing anything except the embedding
//! itself.

use adaptive_pathway::background::reembed_stale_beliefs;
use adaptive_pathway::config::{Config, EmbeddingConfig};
use adaptive_pathway::engine::PathwayEngine;
use adaptive_pathway::store::beliefs::{Belief, Layer, Provenance};
use chrono::Utc;

fn belief_with_model(id: &str, model: &str, at: chrono::DateTime<Utc>) -> Belief {
    Belief {
        id: id.into(),
        text: format!("belief {id}"),
        embedding: vec![1.0, 0.0],
        confidence: 0.6,
        provenance: Provenance::DirectStatement,
        layer: Layer::Context,
        tested: true,
        domain: None,
        tier: "context".into(),
        support_count: 2,
        distinct_sessions: 2,
        contradict_count: 0,
        pinned: false,
        last_confirmed_at: Some(at),
        consolidated_at: None,
        created_at: at,
        updated_at: at,
        session_id: None,
        embedding_model: model.into(),
    }
}

fn config_for(ollama_url: String, model: &str) -> Config {
    Config {
        embedding: EmbeddingConfig {
            ollama_url,
            ollama_model: model.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn a_stale_tagged_belief_is_excluded_from_recall_candidates() {
    // No live Ollama needed for this one -- it only exercises the filter,
    // never calls `embed()`.
    let cfg = config_for("http://127.0.0.1:1".into(), "new-model");
    let engine = PathwayEngine::open_in_memory(cfg).await.unwrap();
    let now = Utc::now();
    engine.db.insert_belief(&belief_with_model("stale", "old-model", now)).await.unwrap();
    engine.db.insert_belief(&belief_with_model("current", "new-model", now)).await.unwrap();

    let candidates = engine.db.list_recall_candidates("s1", "new-model").await.unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].id, "current");

    let block = engine.recall("s1", "").await.expect("the current-model belief must still recall");
    assert!(block.contains("belief current"));
    assert!(!block.contains("belief stale"));
}

#[tokio::test]
async fn reembed_pass_restores_eligibility_without_disturbing_other_fields() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/api/embeddings")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"embedding": [0.1, 0.2, 0.3]}"#)
        .expect_at_least(1)
        .create_async()
        .await;

    let cfg = config_for(server.url(), "new-model");
    let embedding_dim = cfg.embedding_dim;
    let engine = PathwayEngine::open_in_memory(cfg).await.unwrap();
    let now = Utc::now() - chrono::Duration::days(3); // distinct from "now" so an accidental touch is detectable
    let before = belief_with_model("b1", "old-model", now);
    engine.db.insert_belief(&before).await.unwrap();

    // (a) excluded before migration
    assert!(engine.db.list_recall_candidates("s1", "new-model").await.unwrap().is_empty());

    // (b) the pass re-embeds and re-tags it
    reembed_stale_beliefs(&engine).await.unwrap();
    let after = engine.db.get_belief("b1").await.unwrap().unwrap();
    assert_eq!(after.embedding_model, "new-model");
    assert_eq!(after.embedding.len(), embedding_dim, "re-embedded vector must be projected to the configured dimension");
    assert_ne!(after.embedding, before.embedding);

    // (c) nothing else moved -- especially not the decay-relevant fields
    assert_eq!(after.updated_at, before.updated_at);
    assert_eq!(after.last_confirmed_at, before.last_confirmed_at);
    assert_eq!(after.confidence, before.confidence);
    assert_eq!(after.support_count, before.support_count);
    assert_eq!(after.distinct_sessions, before.distinct_sessions);
    assert_eq!(after.tested, before.tested);

    // recall-eligible again
    assert_eq!(engine.db.list_recall_candidates("s1", "new-model").await.unwrap().len(), 1);
}

#[tokio::test]
async fn reembed_pass_is_a_no_op_once_nothing_is_stale() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/api/embeddings")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"embedding": [0.1, 0.2, 0.3]}"#)
        // 2 calls total from the *first* pass only: one `probe_ollama()`
        // reachability check, one real `embed()` for the single stale
        // belief. The second pass must add zero more -- `list_stale_
        // embedding_beliefs` comes back empty and `reembed_stale_beliefs`
        // returns before ever probing again.
        .expect(2)
        .create_async()
        .await;

    let cfg = config_for(server.url(), "new-model");
    let engine = PathwayEngine::open_in_memory(cfg).await.unwrap();
    let now = Utc::now();
    engine.db.insert_belief(&belief_with_model("b1", "old-model", now)).await.unwrap();

    reembed_stale_beliefs(&engine).await.unwrap();
    let after_first = engine.db.get_belief("b1").await.unwrap().unwrap();
    assert_eq!(after_first.embedding_model, "new-model");

    // (d) a second pass finds nothing stale left and must not call embed()
    // again -- `mock.expect(1)` above is the real assertion here.
    reembed_stale_beliefs(&engine).await.unwrap();
    let after_second = engine.db.get_belief("b1").await.unwrap().unwrap();
    assert_eq!(after_second.embedding, after_first.embedding, "an already-current belief must not be re-embedded again");

    mock.assert_async().await;
}

#[tokio::test]
async fn reembed_pass_skips_entirely_when_ollama_is_unreachable() {
    // No mock server at all -- probe_ollama() must fail, and the pass must
    // be a clean no-op rather than falling back to the lexical hashing
    // embedder and mislabeling the result as `current_model` (which would
    // silently mix two incompatible embedding spaces under one tag).
    let cfg = config_for("http://127.0.0.1:1".into(), "new-model");
    let engine = PathwayEngine::open_in_memory(cfg).await.unwrap();
    let now = Utc::now();
    let before = belief_with_model("b1", "old-model", now);
    engine.db.insert_belief(&before).await.unwrap();

    reembed_stale_beliefs(&engine).await.unwrap();

    let after = engine.db.get_belief("b1").await.unwrap().unwrap();
    assert_eq!(after.embedding_model, "old-model", "must remain untouched, not silently re-tagged via a hash fallback");
    assert_eq!(after.embedding, before.embedding);
}

#[tokio::test]
async fn sync_embedding_model_fingerprint_never_touches_belief_rows() {
    // `PathwayEngine::open`/`open_in_memory` call this on every construction
    // -- it must only ever write `app_settings`, never a `beliefs` row, so a
    // belief inserted before the "model changed" fingerprint sync stays
    // exactly as it was (only the background re-embed pass may change it).
    let db = adaptive_pathway::store::Db::open_in_memory().await.unwrap();
    let now = Utc::now();
    let before = belief_with_model("b1", "old-model", now);
    db.insert_belief(&before).await.unwrap();

    let changed = db.sync_embedding_model_fingerprint("new-model").await.unwrap();
    assert!(changed, "first run must report a change");
    let after = db.get_belief("b1").await.unwrap().unwrap();
    assert_eq!(after.embedding_model, before.embedding_model);

    // A repeat sync against the same model is a reported no-op.
    let changed_again = db.sync_embedding_model_fingerprint("new-model").await.unwrap();
    assert!(!changed_again);
}
