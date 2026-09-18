//! Stage 4 asynchronous extraction + DISPUTED edges (plan §15 Phase 6,
//! §3.5/§3.6).
//!
//! Pinned here: pending chunks are invisible until extraction succeeds;
//! failures stay pending and retry only after `retry_backoff_s`; ingest
//! makes zero LLM calls; contradiction candidates come only from the probe
//! window (cross-source preferred, `[min, 0.98]`); disputes need
//! `strength >= dispute_strength_floor`; opening the same pair twice is a
//! no-op and an opened edge lowers the victim's confidence (refresh via
//! core math); tombstoned claims are dropped; suppressed chunks are never
//! extracted; the batch bound, the deterministic decay-profile resolution,
//! and the forward-only watermark.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use memorabilia::config::Config;
use memorabilia::core;
use memorabilia::embed::{hash_embed, HashEmbedder};
use memorabilia::engine::Engine;
use memorabilia::learn::extraction::DrainOutcome;
use memorabilia::learn::IngestInput;
use memorabilia::store::propositions::Proposition;
use memorabilia::store::tombstones::{Suppression, Tombstone};
use memorabilia::store::vectors::SqliteVectorIndex;
use memorabilia::store::Db;
use memorabilia::text::{normalize_text, sha256_hex};
use memorabilia::traits::StructuredChat;

const T0: &str = "2026-08-01T10:00:00Z";
const T0_PLUS_60S: &str = "2026-08-01T10:01:00Z";

async fn build_engine(cfg: Config, chat: Arc<dyn StructuredChat>) -> Engine {
    let dim = cfg.embedding_dim;
    let db = Db::open_in_memory().await.unwrap();
    db.init_vectors(dim).await.unwrap();
    let vectors = Arc::new(SqliteVectorIndex::new(db.pool().clone()));
    Engine::new(cfg, db, chat, Arc::new(HashEmbedder::new(dim)), vectors)
}

async fn engine_with_response(
    cfg: Config,
    response: Value,
) -> (Engine, Arc<RecordingChat>) {
    let chat = Arc::new(RecordingChat {
        response,
        calls: Arc::new(AtomicUsize::new(0)),
        last_user_msg: Arc::new(Mutex::new(None)),
    });
    (build_engine(cfg, chat.clone()).await, chat)
}

async fn engine_with(cfg: Config) -> Engine {
    let (engine, _chat) = engine_with_response(cfg, json!({})).await;
    engine
}

/// Counts structured_chat invocations (ingest-makes-no-LLM-calls pin).
struct CountingChat {
    calls: Arc<AtomicUsize>,
    response: Value,
}

#[async_trait]
impl StructuredChat for CountingChat {
    async fn structured_chat(
        &self,
        _messages: Vec<Value>,
        _schema: &Value,
    ) -> Result<Value, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.response.clone())
    }
}

fn doc(content: &str, domain: &str) -> IngestInput {
    IngestInput {
        content: content.into(),
        source_type: "Scraped".into(),
        source_name: domain.into(),
        source_entity: domain.into(),
        captured_at: T0.into(),
        intent: None,
    }
}

fn chunk_id_of(content: &str) -> String {
    format!("c_{}", &sha256_hex(&normalize_text(content))[..24])
}

fn claim(claim: &str, importance: &str, urgency: &str, dc: &str, expiry: &str) -> Value {
    json!({
        "claim": claim,
        "importance": importance,
        "urgency": urgency,
        "decay_class": dc,
        "urgency_expires_at": expiry
    })
}

fn response(claims: Vec<Value>, disputes: Vec<Value>) -> Value {
    json!({"propositions": claims, "disputes": disputes})
}

fn cosine(a: &str, b: &str) -> f64 {
    let dim = Config::default().embedding_dim;
    let (va, vb) = (hash_embed(a, dim), hash_embed(b, dim));
    let dot: f32 = va.iter().zip(&vb).map(|(x, y)| x * y).sum();
    dot as f64
}

/// Captures the user message it is prompted with (probe-window assertions)
/// and counts calls (backoff / never-LLM assertions).
struct RecordingChat {
    response: Value,
    calls: Arc<AtomicUsize>,
    last_user_msg: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl StructuredChat for RecordingChat {
    async fn structured_chat(
        &self,
        messages: Vec<Value>,
        _schema: &Value,
    ) -> Result<Value, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(msg) = messages.iter().rev().find(|m| m["role"] == "user") {
            if let Some(s) = msg.get("content").and_then(|v| v.as_str()) {
                *self.last_user_msg.lock().unwrap() = Some(s.to_string());
            }
        }
        Ok(self.response.clone())
    }
}

/// Fails its first `fail_first` calls, then returns the canned response
/// (retry-after-backoff assertions).
struct FlakyChat {
    fail_first: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
    response: Value,
}

#[async_trait]
impl StructuredChat for FlakyChat {
    async fn structured_chat(
        &self,
        _messages: Vec<Value>,
        _schema: &Value,
    ) -> Result<Value, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_first.fetch_sub(1, Ordering::SeqCst) > 0 {
            return Err("model down (flaky)".into());
        }
        Ok(self.response.clone())
    }
}

/// Test-only extension: an inherent `impl` on `Engine` is illegal outside its
/// crate (E0116), so the helper hangs off a local trait instead.
#[async_trait]
trait EngineTestExt {
    async fn list_pending_len(&self) -> usize;
}

#[async_trait]
impl EngineTestExt for Engine {
    async fn list_pending_len(&self) -> usize {
        self.db.list_pending_chunks(1000).await.unwrap().len()
    }
}

// ---------------------------------------------------------------------------
// Pending → done: visibility, propositions, confidence, links
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pending_chunk_is_invisible_until_extraction_succeeds() {
    let response = response(
        vec![claim("Python lists are mutable", "high", "unknown", "static", "")],
        vec![],
    );
    let (engine, _chat) = engine_with_response(Config::default(), response).await;
    engine.ingest(&doc("python lists are mutable sequence types", "docs.python.org")).await;

    // before extraction: the proposition level does not exist yet
    assert!(engine.db.list_propositions_by_status("active").await.unwrap().is_empty());
    let chunk = engine.db.list_chunks_by_status("active").await.unwrap().pop().unwrap();
    assert_eq!(chunk.extraction_status, "pending");

    let out = engine.drain_extraction(T0).await;
    assert_eq!(
        out,
        DrainOutcome {
            attempted: 1,
            succeeded: 1,
            failed: 0,
            skipped_suppressed: 0,
            propositions_written: 1,
            disputes_opened: 0
        }
    );

    // chunk: done, and the Stage 4 decay profile landed pair-atomic
    let chunk = engine.db.get_chunk(&chunk.chunk_id).await.unwrap().unwrap();
    assert_eq!(chunk.extraction_status, "done");
    assert_eq!(chunk.decay_class, "static");
    assert_eq!(chunk.urgency_expires_at, None);
    assert_eq!(chunk.extraction_error_at, None);

    // proposition: scored strictly from its active support (plan §8) —
    // single source, decay is 1.0 at t == anchor
    let props = engine.db.list_propositions_by_status("active").await.unwrap();
    assert_eq!(props.len(), 1);
    let p = &props[0];
    assert_eq!(p.claim, "Python lists are mutable");
    assert_eq!(p.importance, "high");
    assert_eq!(p.urgency, "unknown");
    assert!(!p.is_disputed);
    assert_eq!(p.urgency_expires_at, None);
    assert_eq!(p.last_assessed_at, Some(T0.to_string()));
    let src = engine
        .db
        .get_source("docs.python.org")
        .await
        .unwrap()
        .unwrap();
    let expected = core::confidence(
        engine.config.correlation_lambda,
        &[(
            src.source_entity.clone(),
            vec![core::effective_weight(src.reliability, 0.0) * 1.0],
        )],
    );
    assert!((p.confidence - expected).abs() < 1e-9);

    // support link: the chunk supports exactly this proposition
    let supporters = engine.db.list_active_supporting_chunks(&p.node_id).await.unwrap();
    assert_eq!(supporters.len(), 1);
    assert_eq!(supporters[0].chunk_id, chunk.chunk_id);
    assert!(engine.db.list_pending_chunks(10).await.unwrap().is_empty());
}

#[tokio::test]
async fn ingest_makes_no_llm_calls() {
    let calls = Arc::new(AtomicUsize::new(0));
    let chat = Arc::new(CountingChat {
        calls: calls.clone(),
        response: json!({}),
    });
    let engine = build_engine(Config::default(), chat).await;
    engine
        .ingest(&doc("python lists are mutable sequence types", "docs.python.org"))
        .await;
    assert_eq!(
        engine.list_pending_len().await,
        1,
        "ingest must not run extraction (plan §15 Phase 4 exit)"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "ingest performs zero LLM calls"
    );
    // the drain is the only path that talks to the model
    let out = engine.drain_extraction(T0).await;
    assert_eq!(out.attempted, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failure_stays_pending_and_retries_after_backoff() {
    let content = "python lists are mutable sequence types";
    let dim = Config::default().embedding_dim;
    let db = Db::open_in_memory().await.unwrap();
    db.init_vectors(dim).await.unwrap();
    let vectors = Arc::new(SqliteVectorIndex::new(db.pool().clone()));
    let chat = Arc::new(FlakyChat {
        fail_first: Arc::new(AtomicUsize::new(1)),
        calls: Arc::new(AtomicUsize::new(0)),
        response: response(vec![claim("Python lists are mutable", "high", "unknown", "static", "")], vec![]),
    });
    let engine = Engine::new(
        Config::default(),
        db,
        chat.clone(),
        Arc::new(HashEmbedder::new(dim)),
        vectors,
    );
    engine.ingest(&doc(content, "docs.python.org")).await;
    let chunk_id = chunk_id_of(content);

    // failure → pending + backoff clock started
    let out = engine.drain_extraction(T0).await;
    assert_eq!(out, DrainOutcome { attempted: 1, succeeded: 0, failed: 1, ..Default::default() });
    let chunk = engine.db.get_chunk(&chunk_id).await.unwrap().unwrap();
    assert_eq!(chunk.extraction_status, "pending");
    assert_eq!(chunk.extraction_error_at, Some(T0.to_string()));

    // inside the backoff window (default 60s): not retry-eligible
    let out = engine.drain_extraction(T0).await;
    assert_eq!(out, DrainOutcome::default());
    assert_eq!(chat.calls.load(Ordering::SeqCst), 1, "no LLM call during backoff");

    // backoff elapsed → retry, and this time the model answers
    let out = engine.drain_extraction(T0_PLUS_60S).await;
    assert_eq!(
        out,
        DrainOutcome {
            attempted: 1,
            succeeded: 1,
            failed: 0,
            propositions_written: 1,
            ..Default::default()
        }
    );
    assert_eq!(chat.calls.load(Ordering::SeqCst), 2);
    let chunk = engine.db.get_chunk(&chunk_id).await.unwrap().unwrap();
    assert_eq!(chunk.extraction_status, "done");
    assert_eq!(chunk.extraction_error_at, None);
}

// ---------------------------------------------------------------------------
// DISPUTED edges: probe window, floor, idempotency, victim refresh
// ---------------------------------------------------------------------------

const TEXT_A: &str = "the production database replica lagged behind the primary by ten minutes";
const TEXT_B: &str = "the production database primary failed over to the replica automatically";

#[tokio::test]
async fn opened_edge_lowers_victim_confidence_and_reopen_is_a_noop() {
    assert!(cosine(TEXT_A, TEXT_B) < 0.98, "test texts must not be near-duplicates");
    let mut cfg = Config::default();
    cfg.conflict_probe_similarity_min = 0.0; // window = [0, 0.98): A is B's neighbor
    let a_id = chunk_id_of(TEXT_A);
    let b_id = chunk_id_of(TEXT_B);
    // Per-chunk responses through one deterministic router: A extracts a
    // claim, B extracts nothing but disputes A at strength 0.8 (>= floor 0.5).
    let engine = per_chunk_engine(cfg.clone()).await;
    engine.ingest(&doc(TEXT_A, "alpha.example")).await;
    engine.ingest(&doc(TEXT_B, "beta.example")).await;

    let out = engine.drain_extraction(T0).await;
    assert_eq!(out.disputes_opened, 1);
    assert_eq!(out.succeeded, 2);

    // one canonical edge row
    let edges = engine.db.list_open_disputed_edges().await.unwrap();
    assert_eq!(edges.len(), 1);
    let (lo, hi) = if a_id < b_id { (&a_id, &b_id) } else { (&b_id, &a_id) };
    assert_eq!(
        (edges[0].chunk_a.as_str(), edges[0].chunk_b.as_str()),
        (lo.as_str(), hi.as_str())
    );
    assert!((edges[0].strength - 0.8).abs() < 1e-9);

    // the victim's proposition was refreshed in the same transaction:
    // its supporting chunk now carries dispute_strength 0.8
    let props = engine.db.list_propositions_by_status("active").await.unwrap();
    assert_eq!(props.len(), 1);
    assert!(props[0].is_disputed);
    let src = engine.db.get_source("alpha.example").await.unwrap().unwrap();
    let expected = core::confidence(
        engine.config.correlation_lambda,
        &[(
            src.source_entity.clone(),
            vec![core::effective_weight(src.reliability, 0.8) * 1.0],
        )],
    );
    assert!((props[0].confidence - expected).abs() < 1e-9);
    assert!(expected < src.reliability, "the dispute must lower the confidence");

    // re-extract B (simulated retry): the same pair re-opens as a no-op
    sqlx::query("UPDATE chunks SET extraction_status = 'pending' WHERE chunk_id = ?")
        .bind(&b_id)
        .execute(engine.db.pool())
        .await
        .unwrap();
    let out = engine.drain_extraction(T0).await;
    assert_eq!(out.disputes_opened, 0, "re-open of an existing pair is a no-op");
    assert_eq!(out.succeeded, 1);
    assert_eq!(engine.db.list_open_disputed_edges().await.unwrap().len(), 1);
}

/// Drives per-chunk canned responses (A extracts, B disputes) through one
/// deterministic router.
async fn per_chunk_engine(cfg: Config) -> Engine {
    let dim = cfg.embedding_dim;
    let db = Db::open_in_memory().await.unwrap();
    db.init_vectors(dim).await.unwrap();
    let vectors = Arc::new(SqliteVectorIndex::new(db.pool().clone()));
    let router = Arc::new(PerChunkRouter {
        for_b: response(
            vec![],
            vec![json!({
                "chunk_id": chunk_id_of(TEXT_A),
                "strength": 0.8,
                "reason": "failure over vs lag are not the same"
            })],
        ),
    });
    Engine::new(
        cfg,
        db,
        router.clone(),
        Arc::new(HashEmbedder::new(dim)),
        vectors,
    )
}

struct PerChunkRouter {
    for_b: Value,
}

#[async_trait]
impl StructuredChat for PerChunkRouter {
    async fn structured_chat(
        &self,
        messages: Vec<Value>,
        _schema: &Value,
    ) -> Result<Value, String> {
        let user = messages
            .iter()
            .rev()
            .find(|m| m["role"] == "user")
            .and_then(|m| m.get("content"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // Key on the CHUNK header (the extraction target), NOT on B's text:
        // B is also rendered inside A's POTENTIAL_CONFLICTS, so a bare
        // substring match would misroute A's own extraction to B's response.
        if user.contains("CHUNK (source_entity: beta.example)") {
            Ok(self.for_b.clone())
        } else {
            Ok(response(
                vec![claim("The replica lags ten minutes", "high", "unknown", "static", "")],
                vec![],
            ))
        }
    }
}

#[tokio::test]
async fn in_window_dispute_below_floor_is_not_opened() {
    const C: &str = "garden soil needs compost mixed in every spring season";
    assert!(cosine(TEXT_A, C) < 0.98);
    let mut cfg = Config::default();
    cfg.conflict_probe_similarity_min = 0.0;
    let c_id = chunk_id_of(C);
    let a_id = chunk_id_of(TEXT_A);
    let (engine, _chat) = engine_with_response(
        cfg,
        response(
            vec![],
            vec![json!({ "chunk_id": a_id, "strength": 0.3, "reason": "mild tension" })],
        ),
    )
    .await;
    engine.ingest(&doc(TEXT_A, "alpha.example")).await;
    engine.ingest(&doc(C, "gamma.example")).await;

    let out = engine.drain_extraction(T0).await;
    assert_eq!(out.disputes_opened, 0, "0.3 < dispute_strength_floor 0.5");
    assert_eq!(out.succeeded, 2);
    assert!(engine.db.list_open_disputed_edges().await.unwrap().is_empty());
    let c = engine.db.get_chunk(&c_id).await.unwrap().unwrap();
    assert_eq!(c.extraction_status, "done", "a floor-rejected dispute never blocks the chunk");
}

#[tokio::test]
async fn out_of_window_dispute_is_rejected() {
    // probe window [0.99, 0.98] is empty: no chunk can be a probe neighbor,
    // so any dispute target is out of the window and must be rejected
    let mut cfg = Config::default();
    cfg.conflict_probe_similarity_min = 0.99;
    let (engine, _chat) = engine_with_response(
        cfg,
        response(
            vec![],
            vec![
                json!({ "chunk_id": chunk_id_of(TEXT_A), "strength": 0.9, "reason": "hallucinated conflict" })
            ],
        ),
    )
    .await;
    engine.ingest(&doc(TEXT_A, "alpha.example")).await;
    engine.ingest(&doc(TEXT_B, "beta.example")).await;

    let out = engine.drain_extraction(T0).await;
    assert_eq!(out.disputes_opened, 0, "a non-probe chunk_id is never a valid dispute target");
    assert_eq!(out.succeeded, 2, "rejection is soft: both chunks still drain");
    assert!(engine.db.list_open_disputed_edges().await.unwrap().is_empty());
}

#[tokio::test]
async fn probe_prefers_cross_source_and_renders_known_propositions() {
    const X: &str = "the backup job ran successfully during the nightly maintenance window";
    const B2: &str = "the backup job failed twice during the nightly maintenance window";
    for pair in [(X, TEXT_A), (X, B2), (TEXT_A, B2)] {
        assert!(cosine(pair.0, pair.1) < 0.98, "test texts must stay in the probe window");
    }
    let mut cfg = Config::default();
    cfg.conflict_probe_similarity_min = 0.0;
    let x_id = chunk_id_of(X);
    let a_id = chunk_id_of(TEXT_A);
    let (engine, chat) = engine_with_response(cfg, json!({})).await;
    engine.ingest(&doc(TEXT_A, "alpha.example")).await;
    engine.ingest(&doc(X, "x.example")).await;
    engine.ingest(&doc(B2, "x.example")).await;

    // A is already extracted: give it an active proposition the prompt
    // must render under its POTENTIAL_CONFLICTS block
    let a_chunk = engine.db.get_chunk(&a_id).await.unwrap().unwrap();
    let a_node = "p_render_test_1";
    engine
        .db
        .insert_proposition(&Proposition {
            node_id: a_node.into(),
            claim: "The replica lags ten minutes".into(),
            confidence: 0.1,
            is_disputed: false,
            status: "active".into(),
            importance: "high".into(),
            urgency: "unknown".into(),
            urgency_expires_at: None,
            last_assessed_at: Some(T0.into()),
            archived_at: None,
            created_at: T0.into(),
        })
        .await
        .unwrap();
    engine.db.add_support_link(&a_chunk.chunk_id, a_node, T0).await.unwrap();

    let out = engine.drain_extraction(T0).await;
    assert_eq!(out.succeeded, 3);

    // The last chunk drained is B2 (x.example). Its probe neighbors are A
    // (alpha.example, cross-source) and X (x.example, same-source). Cross-
    // source neighbors render first, and each neighbor arrives with its own
    // known propositions.
    let msg = chat
        .last_user_msg
        .lock()
        .unwrap()
        .clone()
        .expect("extraction prompted the model");
    let idx_cross = msg
        .find(&format!("chunk_id: {a_id}"))
        .expect("cross-source neighbor A rendered");
    let idx_same = msg
        .find(&format!("chunk_id: {x_id}"))
        .expect("same-source neighbor X rendered");
    assert!(
        idx_cross < idx_same,
        "cross-source chunks come first in POTENTIAL_CONFLICTS"
    );
    // A's block carries its extracted claim with importance; X has none yet.
    let a_block = &msg[idx_cross..idx_same];
    assert!(a_block.contains("The replica lags ten minutes (importance: high)"));
    let x_block = &msg[idx_same..];
    assert!(
        x_block.contains("(none yet)"),
        "same-source neighbor has no extracted propositions"
    );
}

// ---------------------------------------------------------------------------
// Privacy interactions: tombstones, suppressions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tombstoned_claim_is_dropped_others_kept() {
    // The tombstone hashes the claim's normalized text; `normalize_text` is
    // case-preserving, so the tombstoned phrase must match the claim's case.
    let secret = "The secret password is hunter two";
    let (engine, _chat) = engine_with_response(
        Config::default(),
        response(
            vec![
                claim("The secret password is hunter two", "high", "unknown", "static", ""),
                claim("The sky is blue", "low", "unknown", "static", ""),
            ],
            vec![],
        ),
    )
    .await;
    engine
        .db
        .insert_tombstone(&Tombstone {
            text_hash: sha256_hex(&normalize_text(secret)),
            kind: "proposition".into(),
            permanent: true,
            created_at: T0.into(),
        })
        .await
        .unwrap();
    assert_eq!(engine.ingest(&doc("several facts in one chunk", "alpha.example")).await.chunks_written, 1);

    let out = engine.drain_extraction(T0).await;
    assert_eq!((out.succeeded, out.propositions_written), (1, 1));
    let props = engine.db.list_propositions_by_status("active").await.unwrap();
    assert_eq!(props.len(), 1);
    assert_eq!(props[0].claim, "The sky is blue");
}

#[tokio::test]
async fn suppressed_chunk_is_drained_without_llm_or_writes() {
    let content = "python lists are mutable sequence types";
    let (engine, _chat) = engine_with_response(
        Config::default(),
        response(
            vec![claim("This text was marked wrong", "high", "unknown", "static", "")],
            vec![],
        ),
    )
    .await;
    engine.ingest(&doc(content, "alpha.example")).await;
    let chunk_id = chunk_id_of(content);
    engine
        .db
        .insert_suppression(&Suppression {
            chunk_id: chunk_id.clone(),
            reason: "wrong".into(),
            permanent: true,
            expires_at: None,
            created_at: T0.into(),
        })
        .await
        .unwrap();

    let out = engine.drain_extraction(T0).await;
    assert_eq!(
        out,
        DrainOutcome {
            attempted: 1,
            succeeded: 1,
            skipped_suppressed: 1,
            ..Default::default()
        }
    );
    assert!(
        engine.db.list_propositions_by_status("active").await.unwrap().is_empty(),
        "suppressed text must never be distilled into propositions"
    );
    let chunk = engine.db.get_chunk(&chunk_id).await.unwrap().unwrap();
    assert_eq!(chunk.extraction_status, "done");
}

// ---------------------------------------------------------------------------
// Scheduling: batch bound, profile resolution, watermark
// ---------------------------------------------------------------------------

#[tokio::test]
async fn batch_bound_limits_the_drain() {
    let mut cfg = Config::default();
    cfg.extraction.batch_size = 3;
    let engine = engine_with(cfg).await;
    for i in 0..5 {
        let content = format!("document number {i} carries its own distinct facts here");
        assert_eq!(engine.ingest(&doc(&content, &format!("host{i}.example"))).await.chunks_written, 1);
    }
    assert_eq!(engine.list_pending_len().await, 5);

    let out = engine.drain_extraction(T0).await;
    assert_eq!(out.attempted, 3, "one drain processes at most batch_size chunks");
    assert_eq!(engine.list_pending_len().await, 2);

    let out = engine.drain_extraction(T0).await;
    assert_eq!(out.attempted, 2);
    assert_eq!(engine.list_pending_len().await, 0);
}

#[tokio::test]
async fn deadline_profile_earliest_wins_and_malformed_claims_do_not_wedge() {
    let (engine, _chat) = engine_with_response(
        Config::default(),
        response(
            vec![
                claim("The cert expires Sep 1", "high", "high", "deadline", "2026-09-01T00:00:00Z"),
                claim("The cert expires Aug 15", "high", "high", "deadline", "2026-08-15T00:00:00Z"),
                claim("Broken deadline claim", "high", "high", "deadline", "not-a-date"),
                claim("A durable fact", "medium", "unknown", "static", ""),
            ],
            vec![],
        ),
    )
    .await;
    assert_eq!(engine.ingest(&doc("certificate expiry notes", "docs.example.com")).await.chunks_written, 1);
    let out = engine.drain_extraction(T0).await;
    assert_eq!(out, DrainOutcome { attempted: 1, succeeded: 1, propositions_written: 3, ..Default::default() });

    let chunk = engine.db.list_chunks_by_status("active").await.unwrap().pop().unwrap();
    // earliest valid deadline wins; the malformed claim was dropped, not
    // written and not fatal
    assert_eq!(chunk.decay_class, "deadline");
    assert_eq!(chunk.urgency_expires_at, Some("2026-08-15T00:00:00Z".to_string()));
    let props = engine.db.list_propositions_by_status("active").await.unwrap();
    assert_eq!(props.len(), 3);
    assert!(props.iter().all(|p| p.urgency_expires_at == Some("2026-08-15T00:00:00Z".to_string())));

    // a chunk whose only deadline claim is malformed falls through to
    // static (never a CHECK violation, never wedged pending)
    let (engine2, _chat2) = engine_with_response(
        Config::default(),
        response(
            vec![
                claim("Broken deadline claim", "high", "high", "deadline", "junk"),
                claim("A durable fact", "medium", "unknown", "static", ""),
            ],
            vec![],
        ),
    )
    .await;
    assert_eq!(engine2.ingest(&doc("another certificate note here", "docs.example.com")).await.chunks_written, 1);
    let out2 = engine2.drain_extraction(T0).await;
    assert_eq!(out2.succeeded, 1);
    assert_eq!(out2.propositions_written, 1);
    let chunk2 = engine2.db.list_chunks_by_status("active").await.unwrap().pop().unwrap();
    assert_eq!(chunk2.decay_class, "static");
    assert_eq!(chunk2.urgency_expires_at, None);
}

#[tokio::test]
async fn empty_extraction_marks_done_and_keeps_transient() {
    let engine = engine_with(Config::default()).await;
    assert_eq!(engine.ingest(&doc("nothing to extract from here", "alpha.example")).await.chunks_written, 1);
    let out = engine.drain_extraction(T0).await;
    assert_eq!(
        out,
        DrainOutcome { attempted: 1, succeeded: 1, ..Default::default() }
    );
    let chunk = engine.db.list_chunks_by_status("active").await.unwrap().pop().unwrap();
    assert_eq!(chunk.extraction_status, "done");
    assert_eq!(chunk.decay_class, "transient");
    assert!(engine.db.list_propositions_by_status("active").await.unwrap().is_empty());
}

#[tokio::test]
async fn watermark_advances_forward_only() {
    let engine = engine_with(Config::default()).await;
    assert_eq!(engine.db.read_extraction_watermark().await, 0);

    let c1 = chunk_id_of("one");
    assert_eq!(engine.ingest(&doc("one", "a.example")).await.chunks_written, 1);
    let r1 = engine.db.chunk_rowid(&c1).await.unwrap().unwrap();
    engine.drain_extraction(T0).await;
    assert_eq!(engine.db.read_extraction_watermark().await, r1);

    assert_eq!(engine.ingest(&doc("two", "b.example")).await.chunks_written, 1);
    let c2 = chunk_id_of("two");
    let r2 = engine.db.chunk_rowid(&c2).await.unwrap().unwrap();
    assert!(r2 > r1);
    engine.drain_extraction(T0).await;
    assert_eq!(engine.db.read_extraction_watermark().await, r2);

    // an empty drain never resets the watermark
    let out = engine.drain_extraction(T0).await;
    assert_eq!(out, DrainOutcome::default());
    assert_eq!(engine.db.read_extraction_watermark().await, r2);

    // an unparseable now is a soft no-op
    let out = engine.drain_extraction("garbage").await;
    assert_eq!(out, DrainOutcome::default());
}
