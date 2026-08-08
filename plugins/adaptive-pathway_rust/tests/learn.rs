//! Phase 3 acceptance: extraction via mock StructuredChat, learn-watermark
//! double-count guard, and promotion-gate isolation.

use adaptive_pathway::config::Config;
use adaptive_pathway::engine::PathwayEngine;
use adaptive_pathway::learn::{self, LearnRequest, LearnTrigger};
use adaptive_pathway::store::beliefs::{Layer, Provenance};
use adaptive_pathway::traits::MockChat;
use serde_json::json;
use sqlx::SqlitePool;
use std::str::FromStr;

/// Build a minimal in-memory host `bigtiny.db` with the `messages`/`sessions`
/// tables the learn host-seam reads.
async fn host_pool() -> SqlitePool {
    let options = sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:").unwrap();
    let pool = SqlitePool::connect_with(options).await.unwrap();
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS sessions (\
           id TEXT PRIMARY KEY, status TEXT DEFAULT 'idle', updated_at TEXT DEFAULT (datetime('now')))",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS messages (\
           session_id TEXT, role TEXT, content TEXT, rowid INTEGER PRIMARY KEY AUTOINCREMENT)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

async fn insert_message(pool: &SqlitePool, session: &str, role: &str, content: &str) -> i64 {
    sqlx::query("INSERT INTO messages (session_id, role, content) VALUES (?, ?, ?)")
        .bind(session)
        .bind(role)
        .bind(content)
        .execute(pool)
        .await
        .unwrap()
        .last_insert_rowid()
}

/// A MockChat returning one direct_statement observation.
fn chat_one() -> MockChat {
    MockChat {
        response: json!({
            "observations": [{
                "statement": "The user prefers terse code comments.",
                "provenance": "direct_statement",
                "layer": "context",
                "domain": "coding",
                "evidence": "stated explicitly"
            }],
            "corrections": [],
            "tone": "neutral",
            "open_topics": []
        }),
    }
}

#[tokio::test]
async fn extraction_records_observation_and_advances_watermark() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    let host = host_pool().await;
    let rowid = insert_message(&host, "s1", "user", "I like terse comments.").await;
    let chat = chat_one();

    let outcome = learn::extract_and_record(
        &engine,
        &host,
        &chat,
        LearnRequest {
            session_id: "s1",
            through_rowid: rowid,
            given_chunk: None,
        },
        LearnTrigger::TurnEnd,
    )
    .await
    .unwrap();

    assert_eq!(outcome.observations, 1);

    let watermark = engine.db.last_learned_rowid("s1").await.unwrap();
    assert_eq!(watermark, rowid);

    // The observation became a belief.
    let beliefs = engine.db.list_beliefs(None).await.unwrap();
    assert_eq!(beliefs.len(), 1);
    assert_eq!(
        beliefs[0].layer,
        Layer::Context,
        "extractor must never write identity; context is the top layer it can write"
    );
    assert!(!beliefs[0].tested);
}

#[tokio::test]
async fn no_double_count_on_second_pass() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    let host = host_pool().await;
    let rowid = insert_message(&host, "s1", "user", "I like terse comments.").await;

    let req = LearnRequest {
        session_id: "s1",
        through_rowid: rowid,
        given_chunk: None,
    };
    let chat = chat_one();

    let o1 = learn::extract_and_record(&engine, &host, &chat, req.clone(), LearnTrigger::TurnEnd)
        .await
        .unwrap();
    assert_eq!(o1.observations, 1);

    // Second pass through the same rowid -> watermark guard skips.
    let o2 = learn::extract_and_record(&engine, &host, &chat, req, LearnTrigger::TurnEnd)
        .await
        .unwrap();
    assert_eq!(o2.observations, 0, "double-count guard must skip re-processing");

    let beliefs = engine.db.list_beliefs(None).await.unwrap();
    assert_eq!(beliefs.len(), 1);
}

#[tokio::test]
async fn watermark_never_regresses_on_out_of_order() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    let host = host_pool().await;
    let r1 = insert_message(&host, "s1", "user", "msg one").await;
    let r2 = insert_message(&host, "s1", "user", "msg two").await;
    let chat = chat_one();

    // Learn through r2 first.
    learn::extract_and_record(
        &engine,
        &host,
        &chat,
        LearnRequest { session_id: "s1", through_rowid: r2, given_chunk: None },
        LearnTrigger::TurnEnd,
    )
    .await
    .unwrap();
    assert_eq!(engine.db.last_learned_rowid("s1").await.unwrap(), r2);

    // A compaction pass folding an earlier range (r1) must NOT move it back.
    learn::extract_and_record(
        &engine,
        &host,
        &chat,
        LearnRequest { session_id: "s1", through_rowid: r1, given_chunk: None },
        LearnTrigger::Compaction,
    )
    .await
    .unwrap();
    assert_eq!(
        engine.db.last_learned_rowid("s1").await.unwrap(),
        r2,
        "watermark must never regress"
    );
}

#[tokio::test]
async fn concurrent_second_pass_yields_zero_new() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    let host = host_pool().await;
    let rowid = insert_message(&host, "s1", "user", "context").await;
    let req = LearnRequest { session_id: "s1", through_rowid: rowid, given_chunk: None };

    // Run two passes concurrently; both may attempt, but the per-session
    // learn lock + watermark guard ensure at most one records observations.
    let chat = chat_one();
    let (a, b) = tokio::join!(
        learn::extract_and_record(&engine, &host, &chat, req.clone(), LearnTrigger::TurnEnd),
        learn::extract_and_record(&engine, &host, &chat, req, LearnTrigger::TurnEnd),
    );
    let total = a.unwrap().observations + b.unwrap().observations;
    let beliefs = engine.db.list_beliefs(None).await.unwrap();
    assert!(total >= 1);
    assert_eq!(beliefs.len(), 1, "concurrent passes must not double-insert");
}

#[tokio::test]
async fn promotion_each_gate_blocks_independently() {
    use adaptive_pathway::consolidate::promotion_gates_pass;
    use adaptive_pathway::store::beliefs::Belief;

    let base = Belief {
        id: "b".into(),
        text: "x".into(),
        embedding: vec![1.0, 0.0],
        confidence: 0.7,
        provenance: Provenance::DirectStatement,
        layer: Layer::Context,
        tested: false,
        domain: None,
        tier: "context".into(),
        support_count: 5,
        distinct_sessions: 3,
        contradict_count: 0,
        pinned: false,
        last_confirmed_at: None,
        consolidated_at: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        session_id: None,
        embedding_model: Config::default().embedding.ollama_model,
    };

    // pass when all four gates hold
    assert!(promotion_gates_pass(&base));

    // gate 1: support_count < 3
    let mut b = base.clone();
    b.support_count = 2;
    assert!(!promotion_gates_pass(&b), "support_count gate must block");

    // gate 2: distinct_sessions < 2
    let mut b = base.clone();
    b.distinct_sessions = 1;
    assert!(!promotion_gates_pass(&b), "distinct_sessions gate must block");

    // gate 3: provenance not in {direct, controlled, correction} AND untested
    let mut b = base.clone();
    b.provenance = Provenance::InferredPattern;
    b.tested = false;
    assert!(!promotion_gates_pass(&b), "provenance gate must block");

    // gate 3 alternative: tested bypasses the provenance gate
    let mut b = base.clone();
    b.provenance = Provenance::SingleObservation;
    b.tested = true;
    assert!(promotion_gates_pass(&b), "tested should satisfy the provenance gate");

    // gate 4: confidence < 0.65
    let mut b = base.clone();
    b.confidence = 0.6;
    assert!(!promotion_gates_pass(&b), "confidence gate must block");
}

#[tokio::test]
async fn paused_session_learn_is_skipped() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    engine.set_paused("s1", true).await.unwrap();
    let host = host_pool().await;
    let rowid = insert_message(&host, "s1", "user", "something").await;
    let chat = chat_one();
    let outcome = learn::extract_and_record(
        &engine,
        &host,
        &chat,
        LearnRequest { session_id: "s1", through_rowid: rowid, given_chunk: None },
        LearnTrigger::TurnEnd,
    )
    .await
    .unwrap();
    assert_eq!(outcome.observations, 0);
    assert!(engine.db.list_beliefs(None).await.unwrap().is_empty());
}

/// Regression for issue #8 (the global exchange counter never advanced --
/// no caller ever invoked `bump_global_exchange`, so assumption scheduling
/// could never leave `Scheduled`, meaning `[Worth testing this turn]` could
/// never actually surface): each genuine learn pass bumps the counter.
#[tokio::test]
async fn extract_and_record_bumps_the_global_exchange_counter() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    let host = host_pool().await;
    let chat = chat_one();

    assert_eq!(engine.db.global_exchange_count().await.unwrap(), 0);

    let r1 = insert_message(&host, "s1", "user", "one").await;
    learn::extract_and_record(
        &engine,
        &host,
        &chat,
        LearnRequest { session_id: "s1", through_rowid: r1, given_chunk: None },
        LearnTrigger::TurnEnd,
    )
    .await
    .unwrap();
    assert_eq!(engine.db.global_exchange_count().await.unwrap(), 1);

    let r2 = insert_message(&host, "s1", "user", "two").await;
    learn::extract_and_record(
        &engine,
        &host,
        &chat,
        LearnRequest { session_id: "s1", through_rowid: r2, given_chunk: None },
        LearnTrigger::TurnEnd,
    )
    .await
    .unwrap();
    assert_eq!(engine.db.global_exchange_count().await.unwrap(), 2);

    // A pass that's skipped by the watermark guard (nothing new to learn)
    // must NOT bump the counter -- only genuine learn passes count.
    learn::extract_and_record(
        &engine,
        &host,
        &chat,
        LearnRequest { session_id: "s1", through_rowid: r2, given_chunk: None },
        LearnTrigger::TurnEnd,
    )
    .await
    .unwrap();
    assert_eq!(engine.db.global_exchange_count().await.unwrap(), 2, "a skipped (already-learned) pass must not bump the counter");
}

/// Regression for issue #6: after a daemon restart, `PathwayEngine`'s
/// in-memory `paused_override` map starts empty regardless of what's
/// persisted -- `db.set_paused` (bypassing `engine.set_paused`, which also
/// populates the in-memory map) reproduces exactly that post-restart state:
/// the DB says paused, the in-memory override knows nothing about it. The
/// learn path must still honor the DB-persisted pause.
#[tokio::test]
async fn learn_honors_a_db_only_pause_with_no_in_memory_override() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    engine.db.set_paused("s1", true).await.unwrap();

    let host = host_pool().await;
    let rowid = insert_message(&host, "s1", "user", "something").await;
    let chat = chat_one();
    let outcome = learn::extract_and_record(
        &engine,
        &host,
        &chat,
        LearnRequest { session_id: "s1", through_rowid: rowid, given_chunk: None },
        LearnTrigger::TurnEnd,
    )
    .await
    .unwrap();
    assert_eq!(outcome.observations, 0, "a DB-only pause (no in-memory override) must still be honored");
    assert!(engine.db.list_beliefs(None).await.unwrap().is_empty());
}
