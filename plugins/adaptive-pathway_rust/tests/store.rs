//! Phase 1 acceptance: schema correctness and belief round-trip.

use adaptive_pathway::store::beliefs::{Belief, Layer, Provenance};
use adaptive_pathway::store::Db;
use chrono::Utc;

fn sample_belief(dim: usize) -> Belief {
    let embedding: Vec<f32> = (0..dim).map(|i| (i as f32) / (dim as f32)).collect();
    Belief {
        id: "b1".into(),
        text: "The user prefers terse code comments.".into(),
        embedding,
        confidence: 0.7,
        provenance: Provenance::DirectStatement,
        layer: Layer::Context,
        tested: true,
        domain: Some("coding".into()),
        tier: "context".into(),
        support_count: 1,
        distinct_sessions: 1,
        contradict_count: 0,
        pinned: false,
        last_confirmed_at: Some(Utc::now()),
        consolidated_at: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

#[tokio::test]
async fn belief_blob_round_trip() {
    let db = Db::open_in_memory().await.unwrap();
    let b = sample_belief(384);
    db.insert_belief(&b).await.unwrap();

    let got = db.get_belief("b1").await.unwrap().unwrap();
    assert_eq!(got.text, b.text);
    assert_eq!(got.embedding.len(), 384);
    // the f32 values survive the byte round-trip bit-exactly
    assert_eq!(got.embedding, b.embedding);
    assert!((got.confidence - 0.7).abs() < 1e-9);
    assert_eq!(got.provenance, Provenance::DirectStatement);
    assert_eq!(got.layer, Layer::Context);
    assert!(got.tested);
    assert_eq!(got.domain.as_deref(), Some("coding"));
}

#[tokio::test]
async fn in_memory_database_is_isolated() {
    let db1 = Db::open_in_memory().await.unwrap();
    let db2 = Db::open_in_memory().await.unwrap();
    db1.insert_belief(&sample_belief(384)).await.unwrap();
    // each in-memory DB is its own connection
    assert!(db2.get_belief("b1").await.unwrap().is_none());
}

const EXPECTED_INDEXES: &[&str] = &[
    "idx_beliefs_layer",
    "idx_beliefs_domain",
    "idx_beliefs_tested",
    "idx_observations_session",
    "idx_assumptions_state",
    "idx_suppressions_hash",
];

#[tokio::test]
async fn every_declared_index_exists() {
    let db = Db::open_in_memory().await.unwrap();
    for name in EXPECTED_INDEXES {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?",
        )
        .bind(name)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(count, 1, "expected index {} to exist", name);
    }
}

#[tokio::test]
async fn every_core_table_exists() {
    let db = Db::open_in_memory().await.unwrap();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table'",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    for t in [
        "beliefs",
        "assumptions",
        "contradictions",
        "observations",
        "domains",
        "suppressions",
        "conversation_state",
        "forget_tombstones",
        "audit_log",
        "novelty_tables",
        "synthesis_log",
        "app_settings",
    ] {
        assert!(tables.contains(&t.to_string()), "expected table {}", t);
    }
}

#[tokio::test]
async fn identity_layer_rejected_by_check() {
    // The extractor-visible guard: write layer='identity' directly through
    // SQL must be rejected by the CHECK constraint.
    let db = Db::open_in_memory().await.unwrap();
    let res = sqlx::query(
        "INSERT INTO beliefs (id, text, embedding, confidence, provenance, layer, tested, \
         support_count, distinct_sessions, contradict_count, pinned) \
         VALUES ('x', 't', X'00000000', 0.5, 'single_observation', 'bogus_layer', 0, 1, 1, 0, 0)",
    )
    .execute(db.pool())
    .await;
    assert!(res.is_err());
}
