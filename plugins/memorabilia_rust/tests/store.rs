//! Store layer.
//!
//! Phase 0: migration chain, in-memory cleanliness, transaction atomicity
//! on the single-connection pool.
//!
//! Phase 2 (plan §15): migration 002 creates every §5.1/§5.2/§9.1 table;
//! the sqlite-vec `chunks_vec` vtab is registered and size-consistent;
//! row types + SQL primitives behave; schema-level guards (status pairing,
//! decay pairing, symmetric dispute uniqueness, outbox pair uniqueness,
//! tier/weight CHECKs) reject bad writes; migrations apply idempotently on
//! both file and in-memory databases.

use memorabilia::config::{Config, DEFAULT_EMBEDDING_MODEL, HASH_EMBED_MODEL};
use memorabilia::error::{Error, Result};
use memorabilia::store::audit::AuditEntry;
use memorabilia::store::chunks::Chunk;
use memorabilia::store::documents::Document;
use memorabilia::store::edges::{DisputedEdge, PropositionEdge};
use memorabilia::store::outbox::OutboxEntry;
use memorabilia::store::propositions::Proposition;
use memorabilia::store::registry::Source;
use memorabilia::store::tombstones::{Suppression, Tombstone};
use memorabilia::store::vectors::SqliteVectorIndex;
use memorabilia::store::Db;
use memorabilia::traits::VectorIndex;

// ---------------------------------------------------------------------------
// Phase 0
// ---------------------------------------------------------------------------

#[tokio::test]
async fn open_in_memory_applies_migrations() {
    let db = Db::open_in_memory().await.unwrap();
    let name: String = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'app_settings'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(name, "app_settings");
}

#[tokio::test]
async fn file_db_reopen_is_idempotent_and_persistent() {
    let path = std::env::temp_dir().join(format!("memorabilia_store_{}.db", uuid::Uuid::new_v4()));
    {
        let db = Db::open(path.to_string_lossy().as_ref()).await.unwrap();
        sqlx::query("INSERT INTO app_settings (key, value) VALUES ('k', 'v')")
            .execute(db.pool())
            .await
            .unwrap();
        // Drop the pool: the file-backed DB persists; reopening re-runs the
        // migration runner, which must no-op on already-applied versions.
    }
    let db2 = Db::open(path.to_string_lossy().as_ref()).await.unwrap();
    let v: Option<String> =
        sqlx::query_scalar("SELECT value FROM app_settings WHERE key = 'k'")
            .fetch_one(db2.pool())
            .await
            .unwrap();
    assert_eq!(v, Some("v".into()));
    drop(db2);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

#[tokio::test]
async fn transaction_rolls_back_on_error() {
    let db = Db::open_in_memory().await.unwrap();
    let err: Result<()> = db
        .run_in_transaction(|| async {
            sqlx::query("INSERT INTO app_settings (key, value) VALUES ('doomed', '1')")
                .execute(db.pool())
                .await?;
            Err(Error::Internal("forced rollback".into()))
        })
        .await;
    assert!(matches!(err, Err(Error::Internal(_))));
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM app_settings WHERE key = 'doomed'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn transaction_commits_clean_value() {
    let db = Db::open_in_memory().await.unwrap();
    let v: i64 = db
        .run_in_transaction(|| async {
            sqlx::query("INSERT INTO app_settings (key, value) VALUES ('ok', '1')")
                .execute(db.pool())
                .await?;
            Ok(42)
        })
        .await
        .unwrap();
    assert_eq!(v, 42);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM app_settings WHERE key = 'ok'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(n, 1);
}

// ---------------------------------------------------------------------------
// Phase 2 fixtures
// ---------------------------------------------------------------------------

const T0: &str = "2026-08-01T10:00:00Z";
const T1: &str = "2026-08-02T10:00:00Z";

fn test_document() -> Document {
    Document {
        document_hash: "doc_1".into(),
        source_entity: "slack#infra".into(),
        source_type: "Slack".into(),
        source_name: "#infra".into(),
        captured_at: T0.into(),
        intent_factor: 0.9,
        chunk_count: 0,
        created_at: T0.into(),
    }
}

/// An active, statically-decayed, extraction-pending chunk.
fn test_chunk(id: &str, content_hash: &str, embedding_model: &str) -> Chunk {
    Chunk {
        chunk_id: id.into(),
        content: format!("the {id} fact chunk body with enough words"),
        content_hash: content_hash.into(),
        document_hash: "doc_1".into(),
        source_entity: "slack#infra".into(),
        source_reliability: 0.54,
        provenance_cluster_id: "cluster_1".into(),
        cluster_citation: "Slack: #infra / 2026-08-01".into(),
        status: "active".into(),
        extraction_status: "pending".into(),
        extraction_error_at: None,
        embedding_model: embedding_model.into(),
        decay_class: "static".into(),
        anchor_at: T0.into(),
        urgency_expires_at: None,
        reinforcement_count: 0,
        grounding_count: 0,
        archived_at: None,
        created_at: T0.into(),
    }
}

/// In-memory DB: migrations + `chunks_vec` (default dim) + one document.
async fn seeded_db() -> Db {
    let db = Db::open_in_memory().await.unwrap();
    db.init_vectors(Config::default().embedding_dim)
        .await
        .unwrap();
    db.insert_document(&test_document()).await.unwrap();
    db
}

fn test_proposition(id: &str) -> Proposition {
    Proposition {
        node_id: id.into(),
        claim: format!("claim {id} about the production db ssl cert"),
        confidence: 0.38,
        is_disputed: false,
        status: "active".into(),
        importance: "high".into(),
        urgency: "medium".into(),
        urgency_expires_at: None,
        last_assessed_at: None,
        archived_at: None,
        created_at: T0.into(),
    }
}

// ---------------------------------------------------------------------------
// Schema presence & idempotency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn migration_002_creates_every_plan_table() {
    let db = Db::open_in_memory().await.unwrap();
    db.init_vectors(Config::default().embedding_dim)
        .await
        .unwrap();
    let names: Vec<(String, String)> = sqlx::query_as(
        // GLOB, not LIKE: LIKE's '_' is a single-char wildcard, which would
        // make "NOT LIKE '_%'" match *nothing*.
        "SELECT name, type FROM sqlite_master
         WHERE name NOT GLOB 'sqlite_*' AND name NOT GLOB '_*'
           AND name NOT GLOB 'chunks_vec_*'",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    let mut table_names: Vec<&str> = names
        .iter()
        .filter(|(_, ty)| ty.as_str() == "table")
        .map(|(n, _)| n.as_str())
        .collect();
    table_names.sort();
    let expected = [
        "app_settings",
        "audit_log",
        "chunk_propositions",
        "chunks",
        "chunks_vec",
        "disputed_edges",
        "documents",
        "proposition_edges",
        "propositions",
        "reinforcement_outbox",
        // Added by migration 004 (Kitty's unified per-session incognito).
        "session_pause",
        "source_registry",
        "suppressions",
        "tombstones",
    ];
    assert_eq!(table_names, expected);
}

#[tokio::test]
async fn session_pause_defaults_unpaused_and_round_trips() {
    let db = Db::open_in_memory().await.unwrap();
    // Absent row = not paused (byte-identical to no-memory behavior).
    assert!(!db.is_paused("s1").await.unwrap());
    // Upsert works with no prior row, and toggles back.
    db.set_paused("s1", true).await.unwrap();
    assert!(db.is_paused("s1").await.unwrap());
    db.set_paused("s1", false).await.unwrap();
    assert!(!db.is_paused("s1").await.unwrap());
    // Per-session: pausing one leaves another untouched.
    db.set_paused("s1", true).await.unwrap();
    assert!(!db.is_paused("s2").await.unwrap());
}

#[tokio::test]
async fn schema_primitives_are_idempotent_on_file_reopen() {
    let path = std::env::temp_dir().join(format!("memorabilia_schema_{}.db", uuid::Uuid::new_v4()));
    {
        let db = Db::open(path.to_string_lossy().as_ref()).await.unwrap();
        let dim = Config::default().embedding_dim;
        db.init_vectors(dim).await.unwrap();
        db.init_vectors(dim).await.unwrap(); // vtab creation re-run: no-op
        db.insert_document(&test_document()).await.unwrap(); // chunks FK target
        db.insert_chunk(&test_chunk("c_1", "hash_c1", DEFAULT_EMBEDDING_MODEL))
            .await
            .unwrap();
    }
    // Reopen: migrations re-run as no-ops, vtab recreation re-runs as a
    // no-op, and the earlier data survives.
    let db2 = Db::open(path.to_string_lossy().as_ref()).await.unwrap();
    db2.init_vectors(Config::default().embedding_dim)
        .await
        .unwrap();
    let c = db2.get_chunk("c_1").await.unwrap().expect("chunk persisted");
    assert_eq!(c.chunk_id, "c_1");
    drop(db2);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

// ---------------------------------------------------------------------------
// Vector index on chunks_vec: roundtrip + cross-space isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn vector_roundtrip_and_cross_space_isolation() {
    let db = seeded_db().await;
    let idx = SqliteVectorIndex::new(db.pool().clone());

    db.insert_chunk(&test_chunk("c_sem", "h_sem", DEFAULT_EMBEDDING_MODEL))
        .await
        .unwrap();
    db.insert_chunk(&test_chunk("c_hash", "h_hash", HASH_EMBED_MODEL))
        .await
        .unwrap();

    // The query vector IS the hash chunk's vector: without the model-tag
    // filter the hash row would score cosine 1.0 and beat everything.
    let dim = Config::default().embedding_dim;
    let query = memorabilia::embed::hash_embed("the payment gateway retries three times", dim);
    let sem: Vec<f32> = (0..dim).map(|i| if i == 3 { 1.0 } else { 0.0 }).collect();

    idx.upsert("c_sem", &sem, DEFAULT_EMBEDDING_MODEL)
        .await
        .unwrap();
    idx.upsert("c_hash", &query, HASH_EMBED_MODEL)
        .await
        .unwrap();

    // Semantic query: only the semantic row may surface.
    let hits = idx.search(&query, DEFAULT_EMBEDDING_MODEL, 10).await.unwrap();
    assert_eq!(hits.len(), 1, "hash-space rows must never join a semantic search");
    assert_eq!(hits[0].0, "c_sem");
    assert!(hits[0].1.abs() < 1e-6, "orthogonal vectors: cos ≈ 0, got {}", hits[0].1);

    // Hash query: the hash row, at full score.
    let hits = idx.search(&query, HASH_EMBED_MODEL, 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, "c_hash");
    assert!((hits[0].1 - 1.0).abs() < 1e-5);

    // The reembed_stale candidate query.
    assert_eq!(
        idx.list_by_model(HASH_EMBED_MODEL).await.unwrap(),
        vec!["c_hash".to_string()]
    );

    // Archiving drops the row out of active-space search (plan §3.3).
    db.archive_chunk("c_sem", T1).await.unwrap();
    let hits = idx.search(&query, DEFAULT_EMBEDDING_MODEL, 10).await.unwrap();
    assert!(hits.is_empty());
    // list_by_model is catalog-tag driven, not status driven (the
    // re-embed candidate set tolerates archived rows; deletion does the
    // real cleanup).
    assert_eq!(
        idx.list_by_model(DEFAULT_EMBEDDING_MODEL).await.unwrap(),
        vec!["c_sem".to_string()]
    );
}

#[tokio::test]
async fn vector_upsert_replaces_and_remove_clears() {
    let db = seeded_db().await;
    let idx = SqliteVectorIndex::new(db.pool().clone());
    db.insert_chunk(&test_chunk("c_1", "h_1", DEFAULT_EMBEDDING_MODEL))
        .await
        .unwrap();
    // The vtab column is float[embedding_dim]; the test vectors must match.
    let dim = Config::default().embedding_dim;
    let a: Vec<f32> = (0..dim).map(|i| if i == 0 { 1.0 } else { 0.0 }).collect();
    let b: Vec<f32> = (0..dim).map(|i| if i == 1 { 1.0 } else { 0.0 }).collect();
    idx.upsert("c_1", &a, DEFAULT_EMBEDDING_MODEL).await.unwrap();
    idx.upsert("c_1", &b, DEFAULT_EMBEDDING_MODEL).await.unwrap(); // replace
    let hits = idx.search(&b, DEFAULT_EMBEDDING_MODEL, 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert!((hits[0].1 - 1.0).abs() < 1e-5, "upsert must replace the stored vector");
    idx.remove("c_1").await.unwrap();
    let hits = idx.search(&b, DEFAULT_EMBEDDING_MODEL, 10).await.unwrap();
    assert!(hits.is_empty());
}

// ---------------------------------------------------------------------------
// Chunk schema guards
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chunk_status_pairing_is_enforced() {
    let db = seeded_db().await;
    // active + archived_at set → CHECK violation.
    let mut bad = test_chunk("c_bad1", "hb1", DEFAULT_EMBEDDING_MODEL);
    bad.archived_at = Some(T1.to_string());
    assert!(db.insert_chunk(&bad).await.is_err());
    // archived + archived_at missing → CHECK violation.
    let mut bad = test_chunk("c_bad2", "hb2", DEFAULT_EMBEDDING_MODEL);
    bad.status = "archived".into();
    assert!(db.insert_chunk(&bad).await.is_err());
    // valid archived row.
    let mut ok = test_chunk("c_ok", "ho", DEFAULT_EMBEDDING_MODEL);
    ok.status = "archived".into();
    ok.archived_at = Some(T1.to_string());
    db.insert_chunk(&ok).await.unwrap();
}

#[tokio::test]
async fn chunk_decay_pairing_is_enforced() {
    let db = seeded_db().await;
    // deadline without T_expire → CHECK violation.
    let mut bad = test_chunk("c_bad", "hb", DEFAULT_EMBEDDING_MODEL);
    bad.decay_class = "deadline".into();
    assert!(db.insert_chunk(&bad).await.is_err());
    // non-deadline WITH T_expire → CHECK violation.
    let mut bad = test_chunk("c_bad2", "hb2", DEFAULT_EMBEDDING_MODEL);
    bad.urgency_expires_at = Some("2026-08-20T23:59:59Z".into());
    assert!(db.insert_chunk(&bad).await.is_err());
    // valid deadline row.
    let mut ok = test_chunk("c_ok", "ho", DEFAULT_EMBEDDING_MODEL);
    ok.decay_class = "deadline".into();
    ok.urgency_expires_at = Some("2026-08-20T23:59:59Z".into());
    db.insert_chunk(&ok).await.unwrap();
    // unknown decay class → CHECK violation.
    let mut bad = test_chunk("c_bad3", "hb3", DEFAULT_EMBEDDING_MODEL);
    bad.decay_class = "evergreen".into();
    assert!(db.insert_chunk(&bad).await.is_err());
}

#[tokio::test]
async fn chunk_primitives_roundtrip() {
    let db = seeded_db().await;
    db.insert_chunk(&test_chunk("c_1", "h1", DEFAULT_EMBEDDING_MODEL))
        .await
        .unwrap();
    // duplicate content_hash → UNIQUE violation.
    let dup = test_chunk("c_2", "h1", DEFAULT_EMBEDDING_MODEL);
    assert!(db.insert_chunk(&dup).await.is_err());

    let got = db.get_chunk("c_1").await.unwrap().unwrap();
    assert_eq!(got.content_hash, "h1");
    assert_eq!(got.reinforcement_count, 0);

    db.apply_chunk_counters("c_1", 2, 1, Some(T1)).await.unwrap();
    let got = db.get_chunk("c_1").await.unwrap().unwrap();
    assert_eq!(got.grounding_count, 2);
    assert_eq!(got.reinforcement_count, 1);
    assert_eq!(got.anchor_at, T1);

    db.mark_extraction_done("c_1").await.unwrap();
    assert_eq!(db.list_pending_chunks(10).await.unwrap().len(), 0);
    assert_eq!(db.list_chunks_by_status("active").await.unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// DISPUTED edge guards (plan §9.1)
// ---------------------------------------------------------------------------

fn test_dispute(a: &str, b: &str, id: &str) -> DisputedEdge {
    DisputedEdge {
        edge_id: id.into(),
        chunk_a: a.into(),
        chunk_b: b.into(),
        strength: 0.8,
        opened_at: T0.into(),
        closed_at: None,
        reason: "contradiction".into(),
    }
}

#[tokio::test]
async fn dispute_edges_unique_and_canonical() {
    let db = seeded_db().await;
    db.insert_chunk(&test_chunk("c_01", "h01", DEFAULT_EMBEDDING_MODEL))
        .await
        .unwrap();
    db.insert_chunk(&test_chunk("c_02", "h02", DEFAULT_EMBEDDING_MODEL))
        .await
        .unwrap();

    db.insert_disputed_edge(&test_dispute("c_01", "c_02", "d_1"))
        .await
        .unwrap();
    // Same pair, second edge id → idempotent no-op via the targeted
    // UNIQUE constraint (plan §15 Phase 6: a re-extraction retry re-sends
    // the pair it already opened; the original row, not the replay, wins).
    assert_eq!(
        db.insert_disputed_edge(&test_dispute("c_01", "c_02", "d_2"))
            .await
            .unwrap(),
        0
    );
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM disputed_edges")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(rows, 1);
    // Reversed endpoints → canonical-order CHECK rejection (the pair is
    // already written; an unordered writer is a bug, not a replay).
    assert!(db
        .insert_disputed_edge(&test_dispute("c_02", "c_01", "d_3"))
        .await
        .is_err());
    // Mis-ordered first write → CHECK rejection even when the pair is
    // fresh.
    db.insert_chunk(&test_chunk("c_03", "h03", DEFAULT_EMBEDDING_MODEL))
        .await
        .unwrap();
    assert!(db
        .insert_disputed_edge(&test_dispute("c_03", "c_01", "d_4"))
        .await
        .is_err());

    // dispute_strength input and closure.
    assert_eq!(db.max_open_dispute_strength("c_01").await.unwrap(), Some(0.8));
    db.close_disputed_edge("d_1", T1).await.unwrap();
    assert_eq!(db.max_open_dispute_strength("c_01").await.unwrap(), None);
    assert!(db.list_open_disputed_edges().await.unwrap().is_empty());
    // Closing twice is a no-op, not an error.
    db.close_disputed_edge("d_1", T1).await.unwrap();
}

#[tokio::test]
async fn dispute_strength_bounded_to_unit_interval_only() {
    // The strength floor (dispute_strength_floor, plan §9.1) is business
    // logic: a 0.4 edge is legal at the schema level (only the writer
    // applies the floor), while strength outside (0, 1] is rejected.
    let db = seeded_db().await;
    db.insert_chunk(&test_chunk("c_01", "h01", DEFAULT_EMBEDDING_MODEL))
        .await
        .unwrap();
    db.insert_chunk(&test_chunk("c_02", "h02", DEFAULT_EMBEDDING_MODEL))
        .await
        .unwrap();
    let mut low = test_dispute("c_01", "c_02", "d_low");
    low.strength = 0.4;
    db.insert_disputed_edge(&low).await.unwrap();
    db.insert_chunk(&test_chunk("c_03", "h03", DEFAULT_EMBEDDING_MODEL))
        .await
        .unwrap();
    let mut over = test_dispute("c_01", "c_03", "d_over");
    over.strength = 1.5;
    assert!(db.insert_disputed_edge(&over).await.is_err());
    let mut zero = test_dispute("c_01", "c_03", "d_zero");
    zero.strength = 0.0;
    assert!(db.insert_disputed_edge(&zero).await.is_err());
}

// ---------------------------------------------------------------------------
// Propositions, support links, edges
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proposition_and_support_links_roundtrip() {
    let db = seeded_db().await;
    db.insert_proposition(&test_proposition("p_1")).await.unwrap();

    db.insert_chunk(&test_chunk("c_1", "h1", DEFAULT_EMBEDDING_MODEL))
        .await
        .unwrap();
    db.insert_chunk(&test_chunk("c_3", "h3", DEFAULT_EMBEDDING_MODEL))
        .await
        .unwrap();
    // A chunk from a distinct source entity.
    let mut other = test_chunk("c_2", "h2", DEFAULT_EMBEDDING_MODEL);
    other.source_entity = "wiki#dba".into();
    db.insert_chunk(&other).await.unwrap();

    db.add_support_link("c_1", "p_1", T0).await.unwrap();
    db.add_support_link("c_1", "p_1", T0).await.unwrap(); // idempotent
    db.add_support_link("c_2", "p_1", T0).await.unwrap();
    db.add_support_link("c_3", "p_1", T0).await.unwrap();

    assert_eq!(db.list_active_supporting_chunks("p_1").await.unwrap().len(), 3);
    // c_1 and c_3 share slack#infra: N_active counts DISTINCT entities → 2
    // (the source-correlation principle at the primitive level, plan §8.2).
    assert_eq!(db.count_active_sources("p_1").await.unwrap(), 2);

    db.set_proposition_confidence("p_1", 0.5, true).await.unwrap();
    let p = db.get_proposition("p_1").await.unwrap().unwrap();
    assert_eq!(p.confidence, 0.5);
    assert!(p.is_disputed);

    db.archive_chunk("c_1", T1).await.unwrap();
    assert_eq!(db.list_active_supporting_chunks("p_1").await.unwrap().len(), 2);

    db.archive_proposition("p_1", T1).await.unwrap();
    let p = db.get_proposition("p_1").await.unwrap().unwrap();
    assert_eq!(p.status, "UNSUPPORTED_ARCHIVE");
    assert!(p.archived_at.is_some());

    // confidence > 1 → CHECK rejection.
    let mut bad = test_proposition("p_bad");
    bad.confidence = 1.5;
    assert!(db.insert_proposition(&bad).await.is_err());
}

#[tokio::test]
async fn proposition_edges_typed_weighted_unique() {
    let db = seeded_db().await;
    db.insert_proposition(&test_proposition("p_1")).await.unwrap();
    db.insert_proposition(&test_proposition("p_2")).await.unwrap();

    let edge = PropositionEdge {
        edge_id: "pe_1".into(),
        edge_type: "RHYMES_WITH".into(),
        from_node: "p_1".into(),
        to_node: "p_2".into(),
        weight: 0.5,
        revalidate_at: Some("2026-09-01T00:00:00Z".into()),
        ttl_days: Some(30),
        created_at: T0.into(),
    };
    db.insert_proposition_edge(&edge).await.unwrap();
    // Duplicate (type, from, to) under a second edge id → UNIQUE rejection.
    let mut dup = edge.clone();
    dup.edge_id = "pe_9".into();
    assert!(db.insert_proposition_edge(&dup).await.is_err());
    // Reversed direction is a different ordered triple: allowed at the
    // schema level; the undirected gate is has_proposition_edge (plan §9.3).
    let mut rev = edge.clone();
    rev.edge_id = "pe_2".into();
    rev.from_node = "p_2".into();
    rev.to_node = "p_1".into();
    db.insert_proposition_edge(&rev).await.unwrap();
    assert!(db
        .has_proposition_edge("RHYMES_WITH", "p_1", "p_2")
        .await
        .unwrap());
    assert!(
        !db
            .has_proposition_edge("CAUSES", "p_1", "p_2")
            .await
            .unwrap()
    );
    // Directional listings: pe_1 leaves p_1, pe_2 leaves p_2.
    assert_eq!(db.list_proposition_edges_from("p_1").await.unwrap().len(), 1);
    assert_eq!(db.list_proposition_edges_from("p_2").await.unwrap().len(), 1);

    // weight > 0.8 → CHECK rejection (plan §10.1 activation bound).
    let mut heavy = edge.clone();
    heavy.edge_id = "pe_3".into();
    heavy.weight = 0.9;
    assert!(db.insert_proposition_edge(&heavy).await.is_err());
    // unknown edge type → CHECK rejection.
    let mut unknown = edge.clone();
    unknown.edge_id = "pe_4".into();
    unknown.edge_type = "INSPIRES".into();
    assert!(db.insert_proposition_edge(&unknown).await.is_err());
    // self-edge → CHECK rejection.
    let mut selfy = edge.clone();
    selfy.edge_id = "pe_5".into();
    selfy.to_node = "p_1".into();
    assert!(db.insert_proposition_edge(&selfy).await.is_err());

    db.delete_proposition_edge("pe_2").await.unwrap();
    assert_eq!(db.list_proposition_edges_from("p_2").await.unwrap().len(), 0);
    assert_eq!(db.list_proposition_edges_from("p_1").await.unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Source registry (plan §4.2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn registry_seeds_once_and_drifts() {
    let db = seeded_db().await;
    db.seed_source("slack#infra", "community", 0.6, 0.54, T0)
        .await
        .unwrap();
    // Re-seed touches nothing — drift is not erased by a replay.
    db.seed_source("slack#infra", "community", 0.6, 0.99, T1)
        .await
        .unwrap();
    let s: Source = db.get_source("slack#infra").await.unwrap().unwrap();
    assert_eq!(s.reliability, 0.54);
    // Drift primitive moves the score.
    db.drift_source_reliability("slack#infra", 0.56, T1).await.unwrap();
    let s: Source = db.get_source("slack#infra").await.unwrap().unwrap();
    assert_eq!(s.reliability, 0.56);
    assert_eq!(s.last_drift_at.as_deref(), Some(T1));
    // Drift clamps to [0, 1]: an out-of-range step can never persist.
    db.drift_source_reliability("slack#infra", 1.7, T1).await.unwrap();
    assert_eq!(db.get_source("slack#infra").await.unwrap().unwrap().reliability, 1.0);
    db.drift_source_reliability("slack#infra", -0.4, T1).await.unwrap();
    assert_eq!(db.get_source("slack#infra").await.unwrap().unwrap().reliability, 0.0);
    // Unknown tier → CHECK rejection (tiers are the data-file ladder).
    assert!(db
        .seed_source("wiki#x", "friend", 0.5, 0.5, T0)
        .await
        .is_err());
    assert_eq!(db.list_sources().await.unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Outbox (plan §6.2)
// ---------------------------------------------------------------------------

fn outbox(chunk: &str, event: &str, reinforced: bool) -> OutboxEntry {
    OutboxEntry {
        chunk_id: chunk.into(),
        query_event_id: event.into(),
        grounded: true,
        reinforced,
        created_at: T0.into(),
    }
}

#[tokio::test]
async fn outbox_unique_pair_makes_redelivery_idempotent() {
    let db = seeded_db().await;
    db.insert_chunk(&test_chunk("c_1", "h1", DEFAULT_EMBEDDING_MODEL))
        .await
        .unwrap();

    db.enqueue_outbox(&outbox("c_1", "ev_1", false)).await.unwrap();
    // Crash-replay of the same (chunk, event) pair: the primitive is a
    // no-op and the row count stays 1.
    db.enqueue_outbox(&outbox("c_1", "ev_1", false)).await.unwrap();
    assert_eq!(db.outbox_count().await.unwrap(), 1);
    // A second event is a second row.
    db.enqueue_outbox(&outbox("c_1", "ev_2", true)).await.unwrap();
    assert_eq!(db.outbox_count().await.unwrap(), 2);

    // The constraint itself rejects duplicate pairs (the idempotency is
    // the constraint, not the app code).
    let raw = sqlx::query(
        "INSERT INTO reinforcement_outbox (chunk_id, query_event_id, grounded, reinforced, created_at) \
         VALUES ('c_1', 'ev_1', 1, 0, ?)",
    )
    .bind(T0)
    .execute(db.pool())
    .await;
    assert!(raw.is_err());

    // FIFO drain order, batch cap, then deletion.
    db.enqueue_outbox(&outbox("c_1", "ev_3", true)).await.unwrap();
    let batch = db.next_outbox_batch(2).await.unwrap();
    assert_eq!(batch.len(), 2);
    assert_eq!(batch[0].query_event_id, "ev_1", "FIFO: oldest first");
    assert_eq!(batch[1].query_event_id, "ev_2");
    assert_eq!(batch[0].reinforced, false);
    assert!(batch[1].reinforced);
    db.delete_outbox_pairs("ev_1", &["c_1".into()]).await.unwrap();
    db.delete_outbox_pairs("ev_2", &["c_1".into()]).await.unwrap();
    assert_eq!(db.outbox_count().await.unwrap(), 1);
    let rest = db.next_outbox_batch(10).await.unwrap();
    assert_eq!(rest[0].query_event_id, "ev_3");
}

// ---------------------------------------------------------------------------
// Tombstones & suppressions (plan §12.2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tombstone_blocks_relearn_and_is_idempotent() {
    let db = seeded_db().await;
    let t = Tombstone {
        text_hash: "th_pii".into(),
        kind: "content".into(),
        permanent: true,
        created_at: T0.into(),
    };
    db.insert_tombstone(&t).await.unwrap();
    db.insert_tombstone(&t).await.unwrap(); // re-forget: no-op
    assert!(db.has_tombstone("th_pii").await.unwrap());
    assert!(!db.has_tombstone("th_other").await.unwrap());
    // Unknown kind → CHECK rejection.
    let bad = Tombstone {
        text_hash: "th_bad".into(),
        kind: "rumor".into(),
        permanent: true,
        created_at: T0.into(),
    };
    assert!(db.insert_tombstone(&bad).await.is_err());
}

#[tokio::test]
async fn suppression_ladder_filters_by_reason_and_expiry() {
    let db = seeded_db().await;
    db.insert_chunk(&test_chunk("c_1", "h1", DEFAULT_EMBEDDING_MODEL))
        .await
        .unwrap();
    db.insert_chunk(&test_chunk("c_2", "h2", DEFAULT_EMBEDDING_MODEL))
        .await
        .unwrap();
    db.insert_chunk(&test_chunk("c_3", "h3", DEFAULT_EMBEDDING_MODEL))
        .await
        .unwrap();

    // wrong → permanent; outdated → time-bounded (one in the past, one not).
    db.insert_suppression(&Suppression {
        chunk_id: "c_1".into(),
        reason: "wrong".into(),
        permanent: true,
        expires_at: None,
        created_at: T0.into(),
    })
    .await
    .unwrap();
    db.insert_suppression(&Suppression {
        chunk_id: "c_2".into(),
        reason: "outdated".into(),
        permanent: false,
        expires_at: Some("2026-01-01T00:00:00Z".into()), // long expired
        created_at: T0.into(),
    })
    .await
    .unwrap();
    db.insert_suppression(&Suppression {
        chunk_id: "c_3".into(),
        reason: "outdated".into(),
        permanent: false,
        expires_at: Some("2099-01-01T00:00:00Z".into()), // still active
        created_at: T0.into(),
    })
    .await
    .unwrap();
    // "private" is not a suppression reason (hard delete, plan §12.2).
    assert!(db
        .insert_suppression(&Suppression {
            chunk_id: "c_1".into(),
            reason: "private".into(),
            permanent: true,
            expires_at: None,
            created_at: T0.into(),
        })
        .await
        .is_err());

    let suppressed = db.list_suppressed_chunk_ids(T1).await.unwrap();
    assert_eq!(suppressed, vec!["c_1".to_string(), "c_3".to_string()]);
    // Re-suppressing with the same reason updates bounds, not duplicates.
    db.insert_suppression(&Suppression {
        chunk_id: "c_3".into(),
        reason: "outdated".into(),
        permanent: false,
        expires_at: Some("2020-01-01T00:00:00Z".into()), // now expired
        created_at: T1.into(),
    })
    .await
    .unwrap();
    let suppressed = db.list_suppressed_chunk_ids(T1).await.unwrap();
    assert_eq!(suppressed, vec!["c_1".to_string()]);
}

// ---------------------------------------------------------------------------
// Audit (plan §12.1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn audit_records_category_only_rejections() {
    let db = seeded_db().await;
    db.insert_audit(&AuditEntry {
        id: "a_1".into(),
        event: "rejected:pii".into(),
        category: Some("email".into()),
        // The PII VALUE itself never reaches the log — category only.
        detail: None,
        created_at: T0.into(),
    })
    .await
    .unwrap();
    db.insert_audit(&AuditEntry {
        id: "a_2".into(),
        event: "rejected:pii".into(),
        category: Some("phone".into()),
        detail: None,
        created_at: T1.into(),
    })
    .await
    .unwrap();
    assert_eq!(db.count_audit("rejected:pii").await.unwrap(), 2);
    let rows = db.list_audit_by_event("rejected:pii").await.unwrap();
    assert_eq!(rows[0].category.as_deref(), Some("email"));
    assert_eq!(rows[1].category.as_deref(), Some("phone"));
}

// ---------------------------------------------------------------------------
// Documents (plan §3.1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn document_abort_on_seen_is_idempotent() {
    let db = Db::open_in_memory().await.unwrap();
    db.insert_document(&test_document()).await.unwrap();
    db.insert_document(&test_document()).await.unwrap(); // replay
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM documents")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(n, 1);
    assert!(!matches!(db.get_document("doc_missing").await, Ok(Some(_))));
}
