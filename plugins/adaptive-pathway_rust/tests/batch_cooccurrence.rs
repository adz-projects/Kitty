//! Batch co-occurrence (migration 006): beliefs whose observations share an
//! `extract_and_record` batch pull each other into recall.
//!
//! The whole point is to capture a relation cosine similarity *structurally
//! cannot* see. Co-occurring facts from one problem context -- "the MSRV is
//! 1.70", "the pool caps at 8 connections" -- are semantically distant from
//! each other, which is exactly why they don't already cluster in the recall
//! embedding space. So every fixture here deliberately uses orthogonal
//! embeddings for the siblings: if the test passed via cosine proximity it
//! would be testing nothing.

use adaptive_pathway::config::Config;
use adaptive_pathway::engine::PathwayEngine;
use adaptive_pathway::store::beliefs::{Belief, Layer, Provenance};
use adaptive_pathway::store::observations::Observation;
use chrono::Utc;

fn belief(id: &str, text: &str, emb: Vec<f32>) -> Belief {
    let now = Utc::now();
    Belief {
        id: id.into(),
        text: text.into(),
        embedding: emb,
        confidence: 0.7,
        provenance: Provenance::DirectStatement,
        layer: Layer::Context,
        tested: true,
        domain: None,
        tier: "context".into(),
        support_count: 3,
        distinct_sessions: 2,
        contradict_count: 0,
        pinned: false,
        last_confirmed_at: Some(now),
        consolidated_at: None,
        created_at: now,
        updated_at: now,
        session_id: None,
        embedding_model: Config::default().embedding.ollama_model.clone(),
    }
}

fn observation(id: &str, belief_id: &str, batch: Option<&str>) -> Observation {
    Observation {
        id: id.into(),
        belief_id: Some(belief_id.into()),
        session_id: Some("s1".into()),
        statement: format!("observation {id}"),
        provenance: "direct_statement".into(),
        layer: "context".into(),
        domain: None,
        evidence: None,
        contradicts: None,
        created_at: Utc::now(),
        batch_id: batch.map(|b| b.to_string()),
    }
}

#[tokio::test]
async fn siblings_from_one_batch_are_reported_as_a_pair() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    engine.db.insert_belief(&belief("b1", "MSRV is 1.70.", vec![1.0, 0.0, 0.0])).await.unwrap();
    engine.db.insert_belief(&belief("b2", "Pool caps at 8.", vec![0.0, 1.0, 0.0])).await.unwrap();
    engine.db.insert_observation(&observation("o1", "b1", Some("batch-a"))).await.unwrap();
    engine.db.insert_observation(&observation("o2", "b2", Some("batch-a"))).await.unwrap();

    let pairs = engine
        .db
        .cooccurring_belief_pairs(&["b1".to_string(), "b2".to_string()])
        .await
        .unwrap();
    assert_eq!(pairs, vec![("b1".to_string(), "b2".to_string())]);
}

#[tokio::test]
async fn observations_in_different_batches_are_not_siblings() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    engine.db.insert_belief(&belief("b1", "MSRV is 1.70.", vec![1.0, 0.0, 0.0])).await.unwrap();
    engine.db.insert_belief(&belief("b2", "Pool caps at 8.", vec![0.0, 1.0, 0.0])).await.unwrap();
    engine.db.insert_observation(&observation("o1", "b1", Some("batch-a"))).await.unwrap();
    engine.db.insert_observation(&observation("o2", "b2", Some("batch-b"))).await.unwrap();

    let pairs = engine
        .db
        .cooccurring_belief_pairs(&["b1".to_string(), "b2".to_string()])
        .await
        .unwrap();
    assert!(pairs.is_empty(), "different batches must not produce an edge");
}

#[tokio::test]
async fn a_null_batch_never_produces_edges() {
    // Observations predating migration 006, and single `record` MCP-tool
    // writes, both carry NULL. SQL `NULL = NULL` is never true, but the
    // query also filters explicitly -- assert the behaviour, not the
    // incidental SQL semantics.
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    engine.db.insert_belief(&belief("b1", "MSRV is 1.70.", vec![1.0, 0.0, 0.0])).await.unwrap();
    engine.db.insert_belief(&belief("b2", "Pool caps at 8.", vec![0.0, 1.0, 0.0])).await.unwrap();
    engine.db.insert_observation(&observation("o1", "b1", None)).await.unwrap();
    engine.db.insert_observation(&observation("o2", "b2", None)).await.unwrap();

    let pairs = engine
        .db
        .cooccurring_belief_pairs(&["b1".to_string(), "b2".to_string()])
        .await
        .unwrap();
    assert!(pairs.is_empty(), "batch-less observations must not co-occur");
}

#[test]
fn co_occurrence_reaches_where_cosine_cannot() {
    use adaptive_pathway::config::DiffusionConfig;
    use adaptive_pathway::vector::spread::diffuse_activation;

    // Three mutually orthogonal candidates: no cosine edge can exist between
    // any pair at any threshold. Anchor is index 0 (highest seed score).
    let embs = vec![vec![1.0_f32, 0.0, 0.0], vec![0.0, 1.0, 0.0], vec![0.0, 0.0, 1.0]];
    let scores = vec![0.9, 0.05, 0.05];
    let cfg = DiffusionConfig::default();

    let cosine_only = diffuse_activation(&embs, &scores, &[], &cfg);
    assert_eq!(
        cosine_only,
        vec![0.0, 0.0, 0.0],
        "orthogonal candidates share no cosine edges, so pure diffusion reaches nothing"
    );

    // Same set, but candidate 1 was observed in the same batch as the anchor.
    let cooccurrence = vec![vec![1], vec![0], vec![]];
    let with_batches = diffuse_activation(&embs, &scores, &cooccurrence, &cfg);
    assert!(
        with_batches[1] > 0.0,
        "the batch sibling must receive energy the cosine graph could never carry"
    );
    assert_eq!(
        with_batches[2], 0.0,
        "a candidate with neither a cosine nor a batch edge stays unreached"
    );
}

#[test]
fn co_occurrence_weight_zero_disables_the_pull_without_touching_cosine() {
    use adaptive_pathway::config::DiffusionConfig;
    use adaptive_pathway::vector::spread::diffuse_activation;

    let embs = vec![vec![1.0_f32, 0.0, 0.0], vec![0.0, 1.0, 0.0]];
    let scores = vec![0.9, 0.05];
    let cooccurrence = vec![vec![1], vec![0]];

    let off = DiffusionConfig { cooccurrence_weight: 0.0, ..DiffusionConfig::default() };
    assert_eq!(diffuse_activation(&embs, &scores, &cooccurrence, &off), vec![0.0, 0.0]);
}

#[test]
fn diffusion_with_co_occurrence_is_deterministic() {
    // Selection order feeds the byte-identical-render contract downstream, so
    // an unstable activation vector would surface as a prompt that changes
    // between turns with no state change -- a prompt-prefix cache miss every
    // turn, not just cosmetic churn.
    use adaptive_pathway::config::DiffusionConfig;
    use adaptive_pathway::vector::spread::diffuse_activation;

    let embs = vec![vec![1.0_f32, 0.0, 0.0], vec![0.0, 1.0, 0.0], vec![0.0, 0.0, 1.0]];
    let scores = vec![0.9, 0.4, 0.05];
    let cooccurrence = vec![vec![1, 2], vec![0], vec![0]];
    let cfg = DiffusionConfig::default();

    let a = diffuse_activation(&embs, &scores, &cooccurrence, &cfg);
    let b = diffuse_activation(&embs, &scores, &cooccurrence, &cfg);
    assert_eq!(a, b);
}
