//! Two-pass retrieval, summary+index rendering, on-demand item lookup, and
//! the reinforcement outbox write (plan §10, §6; §15 Phase 7).
//!
//! Pinned here (behavior, not wording): a `pending` chunk is invisible to
//! recall until extraction completes; a higher-cosine seed outranks a
//! lower-cosine one; the injected block never exceeds `injection_token_cap`;
//! every index item_id resolves through `render_item`; recall grounds every
//! rendered chunk but reinforces only undisputed ones; an unknown/archived
//! item never resurfaces.

use std::sync::Arc;

use serde_json::{json, Value};

use memorabilia::config::Config;
use memorabilia::embed::HashEmbedder;
use memorabilia::engine::Engine;
use memorabilia::learn::IngestInput;
use memorabilia::store::edges::DisputedEdge;
use memorabilia::store::vectors::SqliteVectorIndex;
use memorabilia::store::Db;
use memorabilia::text::{normalize_text, sha256_hex};
use memorabilia::traits::MockChat;

const T0: &str = "2026-08-01T10:00:00Z";

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

/// One-proposition-per-chunk canned extraction.
fn one_claim() -> Value {
    json!({
        "propositions": [{
            "claim": "A relevant fact about the topic",
            "importance": "medium",
            "urgency": "unknown",
            "decay_class": "static",
            "urgency_expires_at": ""
        }],
        "disputes": []
    })
}

#[tokio::test]
async fn empty_engine_recalls_nothing() {
    let engine = engine(one_claim()).await;
    assert!(engine.recall("anything at all").await.is_none());
    assert!(engine.search_index("anything", 0, 10).await.is_empty());
}

#[tokio::test]
async fn pending_chunk_is_invisible_until_extraction() {
    let engine = engine(one_claim()).await;
    engine
        .ingest(&doc("databases replication shared topic tokens here", "alpha.example"))
        .await;

    // Ingested but not extracted: the chunk is pending, so recall sees nothing.
    assert!(engine.recall("databases replication topic").await.is_none());

    engine.drain_extraction(T0).await;

    // After extraction the proposition is live and recall surfaces it.
    let block = engine.recall("databases replication topic").await;
    assert!(block.is_some(), "extracted proposition must be recallable");
    assert!(block.unwrap().contains("A relevant fact about the topic"));
}

#[tokio::test]
async fn higher_cosine_seed_outranks_lower() {
    let engine = engine(one_claim()).await;
    // A shares the query's tokens; B is unrelated.
    engine
        .ingest(&doc("databases replication shared query tokens topic", "alpha.example"))
        .await;
    engine
        .ingest(&doc("gardening compost mulch unrelated soil spring", "beta.example"))
        .await;
    engine.drain_extraction(T0).await;

    let idx = engine
        .search_index("databases replication shared query tokens", 0, 10)
        .await;
    assert!(idx.len() >= 1, "at least the closer item must seed");
    // The top-ranked item is backed by the alpha source (the closer chunk).
    assert!(
        idx[0].citation.contains("alpha.example"),
        "higher-cosine seed (alpha) must outrank the unrelated one; got {:?}",
        idx.iter().map(|e| &e.citation).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn index_ids_resolve_through_render_item() {
    let engine = engine(one_claim()).await;
    engine
        .ingest(&doc("databases replication shared topic tokens payload", "alpha.example"))
        .await;
    engine.drain_extraction(T0).await;

    let idx = engine.search_index("databases replication topic", 0, 10).await;
    assert!(!idx.is_empty());
    let item_id = idx[0].item_id.clone();

    let view = engine.render_item(&item_id, 0, 10).await.expect("id resolves");
    assert_eq!(view.item_id, item_id);
    assert_eq!(view.claim, "A relevant fact about the topic");
    assert_eq!(view.total_chunks, 1);
    assert_eq!(view.chunks.len(), 1);
    assert!(view.chunks[0].content.contains("databases replication"));
    assert!(view.chunks[0].citation.contains("alpha.example"));

    // An unknown id never resurfaces anything (plan §13.6).
    assert!(engine.render_item("p_does_not_exist", 0, 10).await.is_none());
}

#[tokio::test]
async fn injection_never_exceeds_token_cap() {
    let mut cfg = Config::default();
    cfg.injection_token_cap = 160; // fits the header + a few items, not all 12
    let dim = cfg.embedding_dim;
    let db = Db::open_in_memory().await.unwrap();
    db.init_vectors(dim).await.unwrap();
    let vectors = Arc::new(SqliteVectorIndex::new(db.pool().clone()));
    let engine = Engine::new(
        cfg.clone(),
        db,
        Arc::new(MockChat { response: one_claim() }),
        Arc::new(HashEmbedder::new(dim)),
        vectors,
    );
    for i in 0..12 {
        let content = format!("shared topic tokens document number {i} with distinct filler words");
        engine.ingest(&doc(&content, &format!("host{i}.example"))).await;
    }
    engine.drain_extraction(T0).await;

    let block = engine.recall("shared topic tokens").await.expect("some memory");
    let approx = block.chars().count().div_ceil(4);
    assert!(
        approx <= cfg.injection_token_cap,
        "injected block ~{approx} tokens exceeds cap {}",
        cfg.injection_token_cap
    );
    // Truncation actually happened: not all 12 items made the cut.
    let rendered = block.matches("- [").count();
    assert!(rendered > 0 && rendered < 12, "expected a truncated index, got {rendered} items");
}

#[tokio::test]
async fn recall_grounds_every_chunk_but_reinforces_only_undisputed() {
    let engine = engine(one_claim()).await;
    // Three chunks; A and B will be disputed against each other, C stays clean.
    let a = "databases replication shared alpha topic tokens";
    let b = "databases replication shared beta topic tokens";
    let c = "databases replication shared gamma topic tokens";
    engine.ingest(&doc(a, "alpha.example")).await;
    engine.ingest(&doc(b, "beta.example")).await;
    engine.ingest(&doc(c, "gamma.example")).await;
    engine.drain_extraction(T0).await;

    // Open a DISPUTED edge between A and B (canonical order).
    let (a_id, b_id) = (chunk_id_of(a), chunk_id_of(b));
    let (lo, hi) = if a_id < b_id { (&a_id, &b_id) } else { (&b_id, &a_id) };
    engine
        .db
        .insert_disputed_edge(&DisputedEdge {
            edge_id: "d_test_ab".into(),
            chunk_a: lo.clone(),
            chunk_b: hi.clone(),
            strength: 0.9,
            opened_at: T0.into(),
            closed_at: None,
            reason: "contradiction".into(),
        })
        .await
        .unwrap();

    engine.recall("databases replication shared topic tokens").await;

    let rows = engine.db.next_outbox_batch(100).await.unwrap();
    assert!(!rows.is_empty(), "recall must ground the rendered chunks");
    // Every rendered chunk is grounded (audit); disputed chunks are grounded
    // but never reinforced (principle 4).
    for row in &rows {
        assert!(row.grounded, "every payload chunk is grounded");
        if row.chunk_id == a_id || row.chunk_id == b_id {
            assert!(!row.reinforced, "a disputed chunk must not be reinforced");
        }
    }
    let c_id = chunk_id_of(c);
    let c_row = rows.iter().find(|r| r.chunk_id == c_id);
    if let Some(c_row) = c_row {
        assert!(c_row.reinforced, "an undisputed grounded chunk is reinforced");
    }
}

#[tokio::test]
async fn recall_is_deterministic() {
    let engine = engine(one_claim()).await;
    engine
        .ingest(&doc("databases replication shared topic tokens", "alpha.example"))
        .await;
    engine
        .ingest(&doc("more shared topic tokens second source", "beta.example"))
        .await;
    engine.drain_extraction(T0).await;

    let a = engine.search_index("shared topic tokens", 0, 10).await;
    let b = engine.search_index("shared topic tokens", 0, 10).await;
    let ids_a: Vec<_> = a.iter().map(|e| &e.item_id).collect();
    let ids_b: Vec<_> = b.iter().map(|e| &e.item_id).collect();
    assert_eq!(ids_a, ids_b, "ranking must be byte-stable across calls");
}
