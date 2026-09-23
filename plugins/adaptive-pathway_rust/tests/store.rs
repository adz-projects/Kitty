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
        session_id: None,
        embedding_model: adaptive_pathway::config::DEFAULT_EMBEDDING_MODEL.into(),
    }
}

/// A migration checksum recorded from a CRLF checkout must not block opening
/// a DB built from an LF checkout (the 0.11.2 memorabilia regression); a real
/// content change must still be rejected.
#[tokio::test]
async fn line_ending_only_checksum_drift_is_reconciled_but_edits_are_not() {
    use sha2::{Digest, Sha384};

    let path = std::env::temp_dir().join(format!("pathway_eol_{}.db", uuid::Uuid::new_v4()));
    let p = path.to_string_lossy().to_string();
    let lf_sql = include_str!("../migrations/001_init.sql").replace("\r\n", "\n");
    let lf_sum = Sha384::digest(lf_sql.as_bytes()).to_vec();
    let crlf_sum = Sha384::digest(lf_sql.replace('\n', "\r\n").as_bytes()).to_vec();

    let set_v1 = |sum: Vec<u8>| {
        let p = p.clone();
        async move {
            let db = Db::open(&p).await.unwrap();
            sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = 1")
                .bind(sum)
                .execute(db.pool())
                .await
                .unwrap();
        }
    };

    drop(Db::open(&p).await.unwrap());

    set_v1(crlf_sum).await;
    let db = Db::open(&p).await.expect("CRLF-only drift must not block open");
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 1")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(stored, lf_sum);
    drop(db);

    set_v1(vec![0u8; 48]).await;
    assert!(matches!(
        Db::open(&p).await,
        Err(adaptive_pathway::error::PathwayError::Migrate(_))
    ));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

/// Keys present in `app_settings`, read through a *separate* pool on the same
/// file — i.e. only what was actually committed.
async fn committed_setting_keys(path: &std::path::Path) -> Vec<String> {
    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(path);
    let other = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    let keys: Vec<String> =
        sqlx::query_scalar("SELECT key FROM app_settings WHERE key LIKE 'txn_%' ORDER BY key")
            .fetch_all(&other)
            .await
            .unwrap();
    other.close().await;
    keys
}

fn remove_db_files(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

/// Regression: the agent loop aborts the previous turn's learn task at the
/// start of every turn, and learn writes inside a transaction. Dropping that
/// future between `BEGIN` and `COMMIT` used to strand the single shared
/// connection in an open transaction, so pathway committed nothing from then
/// on. The dropped transaction must be rolled back and later writes commit.
#[tokio::test]
async fn cancelled_transaction_is_rolled_back_and_later_writes_commit() {
    let path = std::env::temp_dir().join(format!("pathway_txn_{}.db", uuid::Uuid::new_v4()));
    let db = Db::open(path.to_string_lossy().as_ref()).await.unwrap();

    let cancelled = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        db.run_in_transaction(|| async {
            sqlx::query("INSERT INTO app_settings (key, value) VALUES ('txn_cancelled', '1')")
                .execute(db.pool())
                .await?;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(())
        }),
    )
    .await;
    assert!(cancelled.is_err(), "the stalled transaction should time out");

    db.run_in_transaction(|| async {
        sqlx::query("INSERT INTO app_settings (key, value) VALUES ('txn_committed', '1')")
            .execute(db.pool())
            .await?;
        Ok(())
    })
    .await
    .expect("a transaction after a cancelled one must succeed");

    assert_eq!(committed_setting_keys(&path).await, vec!["txn_committed".to_string()]);
    drop(db);
    remove_db_files(&path);
}

/// A transaction left open on the connection by any other means is recovered
/// by the next `run_in_transaction` instead of failing it.
#[tokio::test]
async fn stale_open_transaction_is_recovered_by_next_transaction() {
    let path = std::env::temp_dir().join(format!("pathway_stale_{}.db", uuid::Uuid::new_v4()));
    let db = Db::open(path.to_string_lossy().as_ref()).await.unwrap();

    sqlx::query("BEGIN").execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO app_settings (key, value) VALUES ('txn_stale', '1')")
        .execute(db.pool())
        .await
        .unwrap();

    db.run_in_transaction(|| async {
        sqlx::query("INSERT INTO app_settings (key, value) VALUES ('txn_after', '1')")
            .execute(db.pool())
            .await?;
        Ok(())
    })
    .await
    .expect("must recover from the stale transaction");

    assert_eq!(committed_setting_keys(&path).await, vec!["txn_after".to_string()]);
    drop(db);
    remove_db_files(&path);
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
    "idx_beliefs_session",
    "idx_beliefs_embedding_model",
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
