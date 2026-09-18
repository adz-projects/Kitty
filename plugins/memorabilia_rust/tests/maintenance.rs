//! Maintenance tick (plan §11, §15 Phase 8): outbox drain, decay→archive
//! cascade, hard-delete retention, and the persisted heavy-pass gate.
//!
//! Pinned here (behavior, not wording): draining the outbox applies grounding
//! and reinforcement counters exactly once and is idempotent across ticks; a
//! decayed chunk archives and its now-unsupported proposition transitions per
//! importance (medium deletes, high archives); an archived row past the
//! retention window is hard-deleted; the heavy pass runs once per 24h gate.

use std::sync::Arc;

use serde_json::{json, Value};

use memorabilia::config::Config;
use memorabilia::embed::HashEmbedder;
use memorabilia::engine::Engine;
use memorabilia::learn::IngestInput;
use memorabilia::store::vectors::SqliteVectorIndex;
use memorabilia::store::Db;
use memorabilia::text::{normalize_text, sha256_hex};
use memorabilia::traits::MockChat;

const T0: &str = "2026-08-01T10:00:00Z";
const T0_PLUS_30D: &str = "2026-08-31T10:00:00Z";
const T0_PLUS_121D: &str = "2026-11-30T10:00:00Z"; // 30d archive + 91d retention

async fn engine(response: Value) -> Engine {
    let cfg = Config::default();
    let dim = cfg.embedding_dim;
    let db = Db::open_in_memory().await.unwrap();
    db.init_vectors(dim).await.unwrap();
    let vectors = Arc::new(SqliteVectorIndex::new(db.pool().clone()));
    Engine::new(
        cfg,
        db,
        Arc::new(MockChat { response }),
        Arc::new(HashEmbedder::new(dim)),
        vectors,
    )
}

fn doc(content: &str, source: &str) -> IngestInput {
    IngestInput {
        content: content.into(),
        source_type: "Scraped".into(),
        source_name: source.into(),
        source_entity: source.into(),
        captured_at: T0.into(),
        intent: None,
    }
}

fn chunk_id_of(content: &str) -> String {
    format!("c_{}", &sha256_hex(&normalize_text(content))[..24])
}

fn claim(importance: &str) -> Value {
    json!({
        "propositions": [{
            "claim": "A relevant fact about the topic",
            "importance": importance,
            "urgency": "unknown",
            "decay_class": "static",
            "urgency_expires_at": ""
        }],
        "disputes": []
    })
}

#[tokio::test]
async fn outbox_drain_applies_counters_once_and_is_idempotent() {
    let engine = engine(claim("medium")).await;
    let content = "databases replication shared topic tokens payload";
    engine.ingest(&doc(content, "alpha.example")).await;
    engine.drain_extraction(T0).await;
    // Recall enqueues grounding + reinforcement for the supporting chunk.
    engine.recall("databases replication topic").await;
    assert!(engine.db.outbox_count().await.unwrap() > 0);

    // First tick drains and applies the counters.
    let out = engine.maintenance_tick(T0).await;
    assert_eq!(out.outbox_grounded, 1);
    assert_eq!(out.outbox_reinforced, 1);
    assert_eq!(engine.db.outbox_count().await.unwrap(), 0);

    let chunk = engine.db.get_chunk(&chunk_id_of(content)).await.unwrap().unwrap();
    assert_eq!(chunk.grounding_count, 1);
    assert_eq!(chunk.reinforcement_count, 1);

    // A second tick with an empty outbox changes nothing (idempotent).
    let out2 = engine.maintenance_tick(T0).await;
    assert_eq!((out2.outbox_grounded, out2.outbox_reinforced), (0, 0));
    let chunk = engine.db.get_chunk(&chunk_id_of(content)).await.unwrap().unwrap();
    assert_eq!(chunk.grounding_count, 1);
    assert_eq!(chunk.reinforcement_count, 1);
}

#[tokio::test]
async fn static_chunk_does_not_archive_at_30_days() {
    // Negative control: the claim here is `static` (τ = 365d), so at +30d the
    // chunk is nowhere near the prune floor and must stay active.
    let engine = engine(claim("medium")).await;
    let content = "a durable static fact that should not decay in a month";
    engine.ingest(&doc(content, "alpha.example")).await;
    engine.drain_extraction(T0).await;

    let out = engine.maintenance_tick(T0_PLUS_30D).await;
    assert_eq!(out.chunks_archived, 0, "a static chunk does not archive at +30d");
    let chunk = engine.db.get_chunk(&chunk_id_of(content)).await.unwrap().unwrap();
    assert_eq!(chunk.status, "active");
}

#[tokio::test]
async fn transient_chunk_archives_and_cascades_by_importance() {
    // A chunk with no extracted claims stays transient; give it a proposition
    // by extracting a transient-class claim so decay uses τ_transient (7d).
    let response = json!({
        "propositions": [{
            "claim": "A relevant fact about the topic",
            "importance": "medium",
            "urgency": "unknown",
            "decay_class": "transient",
            "urgency_expires_at": ""
        }],
        "disputes": []
    });
    let engine = engine(response).await;
    let content = "transient claim decays below the prune floor after thirty days";
    engine.ingest(&doc(content, "alpha.example")).await;
    engine.drain_extraction(T0).await;
    let chunk_id = chunk_id_of(content);

    // At +30d a transient chunk (τ=7d) has decayed well below the 0.05 floor.
    let out = engine.maintenance_tick(T0_PLUS_30D).await;
    assert_eq!(out.chunks_archived, 1);
    assert_eq!(out.propositions_deleted, 1, "medium importance → deleted, not archived");
    let chunk = engine.db.get_chunk(&chunk_id).await.unwrap().unwrap();
    assert_eq!(chunk.status, "archived");
    assert!(engine.db.list_propositions_by_status("active").await.unwrap().is_empty());
}

#[tokio::test]
async fn high_importance_unsupported_proposition_is_archived_not_deleted() {
    let response = json!({
        "propositions": [{
            "claim": "A high-importance fact worth keeping for forensics",
            "importance": "high",
            "urgency": "unknown",
            "decay_class": "transient",
            "urgency_expires_at": ""
        }],
        "disputes": []
    });
    let engine = engine(response).await;
    let content = "high importance transient claim decays after thirty days here";
    engine.ingest(&doc(content, "alpha.example")).await;
    engine.drain_extraction(T0).await;

    let out = engine.maintenance_tick(T0_PLUS_30D).await;
    assert_eq!(out.chunks_archived, 1);
    assert_eq!(out.propositions_archived, 1, "high importance → UNSUPPORTED_ARCHIVE");
    assert_eq!(out.propositions_deleted, 0);
    let archived = engine.db.list_propositions_by_status("UNSUPPORTED_ARCHIVE").await.unwrap();
    assert_eq!(archived.len(), 1);
}

#[tokio::test]
async fn archived_row_past_retention_is_hard_deleted() {
    let response = json!({
        "propositions": [{
            "claim": "A relevant fact about the topic",
            "importance": "medium",
            "urgency": "unknown",
            "decay_class": "transient",
            "urgency_expires_at": ""
        }],
        "disputes": []
    });
    let engine = engine(response).await;
    let content = "transient claim that will be archived then hard deleted later";
    engine.ingest(&doc(content, "alpha.example")).await;
    engine.drain_extraction(T0).await;
    let chunk_id = chunk_id_of(content);

    // Archive it at +30d.
    engine.maintenance_tick(T0_PLUS_30D).await;
    assert_eq!(
        engine.db.get_chunk(&chunk_id).await.unwrap().unwrap().status,
        "archived"
    );

    // Past the 90-day retention window, the archived chunk is hard-deleted.
    let out = engine.maintenance_tick(T0_PLUS_121D).await;
    assert_eq!(out.chunks_hard_deleted, 1);
    assert!(engine.db.get_chunk(&chunk_id).await.unwrap().is_none());
}

#[tokio::test]
async fn heavy_pass_runs_once_per_24h_gate() {
    let engine = engine(claim("medium")).await;
    // First tick: no anchor yet → heavy runs.
    let a = engine.maintenance_tick(T0).await;
    assert!(a.heavy_ran, "first tick runs the heavy pass");

    // Same-time tick: within the 24h gate → heavy skipped.
    let b = engine.maintenance_tick(T0).await;
    assert!(!b.heavy_ran, "heavy pass is gated for 24h");

    // 25h later → heavy runs again.
    let c = engine.maintenance_tick("2026-08-02T11:00:00Z").await;
    assert!(c.heavy_ran, "heavy pass re-runs after the gate elapses");
}
