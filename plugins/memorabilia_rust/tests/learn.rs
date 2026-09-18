//! Ingestion pipeline (plan §15 Phase 4, §3): Stages 0–3 + reliability
//! seeding.
//!
//! Pinned here: deterministic fixed-window chunking on the configured
//! window, two-tier hash dedup (document abort at Stage 1, content skip at
//! Stage 3 — within and across documents), cluster join at the injected
//! `cluster_similarity_threshold` boundary, the same-citation-never-splits
//! rule, the §3.4 citation/slug format, reliability seeding (tier prior ×
//! intent, seed-once), no LLM calls on the hot path, and tombstone
//! re-learn blocks at Stages 1 and 3.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use memorabilia::config::{Config, HASH_EMBED_MODEL};
use memorabilia::embed::{hash_embed, HashEmbedder};
use memorabilia::engine::Engine;
use memorabilia::learn::{AttachmentIntent, IngestInput, IngestOutcome};
use memorabilia::store::tombstones::Tombstone;
use memorabilia::store::vectors::SqliteVectorIndex;
use memorabilia::store::Db;
use memorabilia::text::{normalize_text, sha256_hex};
use memorabilia::traits::{MockChat, StructuredChat};

const T0: &str = "2026-08-01T10:00:00Z";

async fn engine_with(cfg: Config) -> Engine {
    let dim = cfg.embedding_dim;
    let db = Db::open_in_memory().await.unwrap();
    db.init_vectors(dim).await.unwrap();
    let vectors = Arc::new(SqliteVectorIndex::new(db.pool().clone()));
    Engine::new(
        cfg,
        db,
        Arc::new(MockChat {
            response: serde_json::json!({}),
        }),
        Arc::new(HashEmbedder::new(dim)),
        vectors,
    )
}

fn scrape(content: &str, domain: &str, captured_at: &str) -> IngestInput {
    IngestInput {
        content: content.into(),
        source_type: "Scraped".into(),
        source_name: domain.into(),
        source_entity: domain.into(),
        captured_at: captured_at.into(),
        intent: None,
    }
}

fn slack_note(content: &str, entity: &str, intent: Option<AttachmentIntent>) -> IngestInput {
    IngestInput {
        content: content.into(),
        source_type: "Slack".into(),
        source_name: "#ops".into(),
        source_entity: entity.into(),
        captured_at: T0.into(),
        intent,
    }
}

struct CountingChat {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl StructuredChat for CountingChat {
    async fn structured_chat(
        &self,
        _messages: Vec<Value>,
        _schema: &Value,
    ) -> Result<Value, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(serde_json::json!({}))
    }
}

// ---------------------------------------------------------------------------
// Row shape, citation, dedup
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ingest_writes_pending_transient_chunks_and_citation() {
    let engine = engine_with(Config::default()).await;
    let content = "python lists are mutable sequence types";
    assert_eq!(engine.ingest(&scrape(content, "docs.python.org", T0)).await.chunks_written, 1);

    let chunk = engine
        .db
        .list_chunks_by_status("active")
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(chunk.content, content);
    assert_eq!(chunk.status, "active");
    assert_eq!(chunk.extraction_status, "pending");
    assert_eq!(chunk.decay_class, "transient");
    assert_eq!(chunk.embedding_model, HASH_EMBED_MODEL);
    assert_eq!(chunk.anchor_at, T0);
    assert_eq!(chunk.created_at, T0);
    assert_eq!(chunk.urgency_expires_at, None);
    assert_eq!(chunk.archived_at, None);
    assert_eq!(chunk.reinforcement_count, 0);
    assert_eq!(chunk.grounding_count, 0);
    // §3.4: per-chunk citation carries the capture date; the cluster slug
    // does not
    assert_eq!(chunk.cluster_citation, "Scraped: docs.python.org / 2026-08-01");
    assert_eq!(chunk.provenance_cluster_id, "scraped_docs.python.org");
    assert_eq!(chunk.content_hash, sha256_hex(&normalize_text(content)));
    assert_eq!(chunk.document_hash, sha256_hex(content));

    // the document row is the completion marker, written with the count
    let doc = engine.db.get_document(&sha256_hex(content)).await.unwrap().unwrap();
    assert_eq!(doc.source_type, "Scraped");
    assert_eq!(doc.source_name, "docs.python.org");
    assert_eq!(doc.chunk_count, 1);
    assert_eq!(doc.intent_factor, 1.0);
}

#[tokio::test]
async fn identical_document_aborts_at_stage_one() {
    let engine = engine_with(Config::default()).await;
    let input = scrape("the router firmware update ships on the second", "docs.example.net", T0);
    assert_eq!(engine.ingest(&input).await.chunks_written, 1);
    let out = engine.ingest(&input).await;
    assert!(out.aborted_document_seen);
    assert_eq!(out.chunks_written, 0);
    assert_eq!(
        engine.db.list_chunks_by_status("active").await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn cross_document_content_dedup_skips_overlapping_windows() {
    let mut cfg = Config::default();
    cfg.chunk_chars_min = 100;
    cfg.chunk_chars_max = 200;
    let engine = engine_with(cfg).await;

    let a = "lorem ipsum dolor sit amet ".repeat(10); // 250 chars
    let out_a = engine.ingest(&scrape(&a, "alpha.example.net", T0)).await;
    assert_eq!(out_a.chunks_written, 2); // windows [0..200], [100..250]

    // doc B re-uses A's content and adds a short tail: its first window is a
    // cross-document duplicate (skipped), its second window is new
    let b = format!("{a} trailer here");
    let out_b = engine.ingest(&scrape(&b, "alpha.example.net", T0)).await;
    assert_eq!(out_b.chunks_written, 1);
    assert_eq!(out_b.chunks_skipped, 1);
    assert!(!out_b.aborted_document_seen);
    assert_eq!(
        engine.db.list_chunks_by_status("active").await.unwrap().len(),
        3
    );
}

#[tokio::test]
async fn normalization_dedup_is_case_preserving() {
    let engine = engine_with(Config::default()).await;

    let out_a = engine
        .ingest(&scrape("The  quick  brown fox.", "site.one", T0))
        .await;
    assert_eq!(out_a.chunks_written, 1);
    let content_a = engine
        .db
        .list_chunks_by_status("active")
        .await
        .unwrap()
        .pop()
        .unwrap()
        .content;
    assert_eq!(content_a, "The quick brown fox");

    // same normalized text, different raw payload → content-dedup skip
    let out_b = engine
        .ingest(&scrape("The quick brown fox", "site.two", T0))
        .await;
    assert_eq!(out_b.chunks_written, 0);
    assert_eq!(out_b.chunks_skipped, 1);

    // case is preserved in the hash: a different case is new content
    let out_c = engine
        .ingest(&scrape("the quick brown fox", "site.three", T0))
        .await;
    assert_eq!(out_c.chunks_written, 1);
    assert_eq!(
        engine.db.list_chunks_by_status("active").await.unwrap().len(),
        2
    );
}

#[tokio::test]
async fn empty_content_is_a_byte_identical_noop() {
    let engine = engine_with(Config::default()).await;
    assert_eq!(engine.ingest(&scrape("   ", "site.one", T0)).await, IngestOutcome::default());
    assert_eq!(engine.db.list_chunks_by_status("active").await.unwrap().len(), 0);
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM documents")
            .fetch_one(engine.db.pool())
            .await
            .unwrap();
    assert_eq!(n, 0);
}

// ---------------------------------------------------------------------------
// Clustering
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cluster_join_boundary_around_the_threshold() {
    let a = "the quick brown fox jumps over the lazy dog again and again";
    let b = "the quick brown fox jumps over the lazy dog again and again today";
    let dim = Config::default().embedding_dim;
    let (va, vb) = (hash_embed(a, dim), hash_embed(b, dim));
    let cos: f32 = va.iter().zip(&vb).map(|(x, y)| x * y).sum();
    assert!((0.0..1.0).contains(&cos));

    // threshold just below the pair's cosine → B joins A's cluster
    let mut join_cfg = Config::default();
    join_cfg.cluster_similarity_threshold = cos as f64 - 0.01;
    let engine = engine_with(join_cfg).await;
    assert_eq!(engine.ingest(&scrape(a, "alpha.example", T0)).await.chunks_written, 1);
    assert_eq!(engine.ingest(&scrape(b, "beta.example", T0)).await.chunks_written, 1);
    let chunks = engine.db.list_chunks_by_status("active").await.unwrap();
    let ca = chunks.iter().find(|c| c.content == a).unwrap();
    let cb = chunks.iter().find(|c| c.content == b).unwrap();
    // A created its own source slug; B (a different source) inherits it
    assert_eq!(ca.provenance_cluster_id, "scraped_alpha.example");
    assert_eq!(cb.provenance_cluster_id, "scraped_alpha.example");

    // threshold just above → B keeps its own slug
    let mut split_cfg = Config::default();
    split_cfg.cluster_similarity_threshold = cos as f64 + 0.01;
    let engine = engine_with(split_cfg).await;
    assert_eq!(engine.ingest(&scrape(a, "alpha.example", T0)).await.chunks_written, 1);
    assert_eq!(engine.ingest(&scrape(b, "beta.example", T0)).await.chunks_written, 1);
    let chunks = engine.db.list_chunks_by_status("active").await.unwrap();
    let cb = chunks.iter().find(|c| c.content == b).unwrap();
    assert_eq!(cb.provenance_cluster_id, "scraped_beta.example");
}

#[tokio::test]
async fn same_citation_never_splits_a_cluster() {
    let engine = engine_with(Config::default()).await; // threshold 0.92
    let t1 = "harbor wall bricks were replaced last winter season";
    let t2 = "pasta water needs a large pinch of salt always";
    assert_eq!(engine.ingest(&scrape(t1, "gamma.example", T0)).await.chunks_written, 1);
    assert_eq!(engine.ingest(&scrape(t2, "gamma.example", T0)).await.chunks_written, 1);
    // unrelated content (cos well under 0.92) still lands in the same
    // source's cluster — one citation, one slug
    let chunks = engine.db.list_chunks_by_status("active").await.unwrap();
    assert_eq!(chunks.len(), 2);
    for c in &chunks {
        assert_eq!(c.provenance_cluster_id, "scraped_gamma.example");
        assert_eq!(c.cluster_citation, "Scraped: gamma.example / 2026-08-01");
    }
}

// ---------------------------------------------------------------------------
// Reliability seeding (plan §4.2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reliability_seeding_ladder_and_seed_once() {
    let engine = engine_with(Config::default()).await;

    // scrape: tier by domain; intent None → factor 1.0
    engine
        .ingest(&scrape("python lists are mutable", "rust-lang.github.io", T0))
        .await;
    let s = engine.db.get_source("rust-lang.github.io").await.unwrap().unwrap();
    assert_eq!(s.tier, "primary");
    assert!((s.tier_prior - 0.95).abs() < 1e-9);
    assert!((s.reliability - 0.95).abs() < 1e-9);

    engine.ingest(&scrape("world news report summary", "nytimes.com", T0)).await;
    let s = engine.db.get_source("nytimes.com").await.unwrap().unwrap();
    assert_eq!(s.tier, "established");
    assert!((s.reliability - 0.80).abs() < 1e-9);

    engine.ingest(&scrape("my random blog post draft", "myblog.example", T0)).await;
    let s = engine.db.get_source("myblog.example").await.unwrap().unwrap();
    assert_eq!(s.tier, "personal");
    assert!((s.reliability - 0.35).abs() < 1e-9);

    // attachment: no domain signal → personal tier, intent factor applied
    // (casual 0.5 × 0.35 = 0.175)
    engine
        .ingest(&slack_note("fyi the deploy broke staging again", "slack#ops", Some(AttachmentIntent::Casual)))
        .await;
    let s = engine.db.get_source("slack#ops").await.unwrap().unwrap();
    assert_eq!(s.tier, "personal");
    assert!((s.reliability - 0.175).abs() < 1e-9);
    let chunk = engine
        .db
        .list_chunks_by_status("active")
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.source_entity == "slack#ops")
        .unwrap();
    assert!((chunk.source_reliability - 0.175).abs() < 1e-9);

    // seed-once: a later, higher-intent attachment does not re-seed the
    // drifted value — the new chunk carries the existing registry value
    engine
        .ingest(&slack_note("supporting ticket evidence attached here", "slack#ops", Some(AttachmentIntent::Evidence)))
        .await;
    let s = engine.db.get_source("slack#ops").await.unwrap().unwrap();
    assert!((s.reliability - 0.175).abs() < 1e-9);
    let chunk = engine
        .db
        .get_chunk_by_content_hash(&sha256_hex("supporting ticket evidence attached here"))
        .await
        .unwrap()
        .unwrap();
    assert!((chunk.source_reliability - 0.175).abs() < 1e-9);
}

// ---------------------------------------------------------------------------
// Chunking window + LLM isolation + tombstone blocks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chunking_uses_the_configured_window() {
    let mut cfg = Config::default();
    cfg.chunk_chars_min = 100;
    cfg.chunk_chars_max = 200;
    let engine = engine_with(cfg).await;

    let content: String = (0..84).map(|i| format!("n{i:03} ")).collect(); // 504 chars
    let out = engine.ingest(&scrape(&content, "alpha.example", T0)).await;
    assert_eq!(out.chunks_written, 4); // 200/100 stride over 504 chars
    for c in engine.db.list_chunks_by_status("active").await.unwrap() {
        assert!(c.content.len() <= 200);
    }
}

#[tokio::test]
async fn ingest_performs_no_llm_calls_on_the_hot_path() {
    let cfg = Config::default();
    let dim = cfg.embedding_dim;
    let db = Db::open_in_memory().await.unwrap();
    db.init_vectors(dim).await.unwrap();
    let vectors = Arc::new(SqliteVectorIndex::new(db.pool().clone()));
    let calls = Arc::new(AtomicUsize::new(0));
    let engine = Engine::new(
        cfg,
        db,
        Arc::new(CountingChat {
            calls: Arc::clone(&calls),
        }),
        Arc::new(HashEmbedder::new(dim)),
        vectors,
    );
    assert_eq!(
        engine
            .ingest(&scrape("stage four runs in the background later", "alpha.example", T0))
            .await
            .chunks_written,
        1
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn tombstones_block_relearning_at_stages_one_and_three() {
    let mut cfg = Config::default();
    cfg.chunk_chars_min = 100;
    cfg.chunk_chars_max = 200;
    let engine = engine_with(cfg).await;

    // Stage 3 block: a content-hash tombstone skips the matching window;
    // the sibling window still writes, and the document row seals with
    // the surviving count. (A single-window document whose payload was
    // tombstoned aborts at Stage 1 instead — the content hash IS the
    // document hash there, and the Stage 1 lookup is kind-agnostic by
    // design.)
    let w1 = "xy".repeat(100); // exactly 200 chars = window one
    let content = format!("{w1} plus a distinct second window trailer here");
    engine
        .db
        .insert_tombstone(&Tombstone {
            text_hash: sha256_hex(&normalize_text(&w1)),
            kind: "content".into(),
            permanent: true,
            created_at: T0.into(),
        })
        .await
        .unwrap();
    let out = engine.ingest(&scrape(&content, "alpha.example", T0)).await;
    assert_eq!(out.chunks_written, 1);
    assert_eq!(out.chunks_skipped, 1);
    let doc = engine.db.get_document(&sha256_hex(&content)).await.unwrap().unwrap();
    assert_eq!(doc.chunk_count, 1);

    // Stage 1 block: a document-hash tombstone aborts before any work
    let d = "another one liner about the river bank";
    engine
        .db
        .insert_tombstone(&Tombstone {
            text_hash: sha256_hex(d),
            kind: "document".into(),
            permanent: true,
            created_at: T0.into(),
        })
        .await
        .unwrap();
    let out = engine.ingest(&scrape(d, "alpha.example", T0)).await;
    assert!(out.aborted_document_seen);
    assert_eq!(out.chunks_written, 0);
    // only the Stage 3 survivor exists
    assert_eq!(
        engine.db.list_chunks_by_status("active").await.unwrap().len(),
        1
    );
}
