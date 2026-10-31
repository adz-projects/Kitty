//! Regression coverage for issue #3: `idle_session_ids` must actually
//! compare ages, not report every session as idle immediately.
//!
//! The `sessions` table here mirrors `bigtiny.db`'s real shape: `updated_at`
//! written via bare `CURRENT_TIMESTAMP`/`datetime('now')`, i.e. naive
//! `'YYYY-MM-DD HH:MM:SS'` text -- not RFC3339. A cutoff bound in from a
//! Rust `chrono::DateTime` (RFC3339, `T` separator) would never compare
//! correctly against that.

use adaptive_pathway::background::{idle_sweep, SweepBackoff};
use adaptive_pathway::config::Config;
use adaptive_pathway::engine::PathwayEngine;
use adaptive_pathway::learn::host::idle_session_ids;
use adaptive_pathway::traits::MockChat;
use serde_json::json;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::SqlitePool;
use std::str::FromStr;

async fn host_pool() -> SqlitePool {
    let options = SqliteConnectOptions::from_str("sqlite::memory:").unwrap();
    let pool = SqlitePool::connect_with(options).await.unwrap();
    sqlx::query(
        "CREATE TABLE sessions (id TEXT PRIMARY KEY, status TEXT DEFAULT 'idle', \
         updated_at TEXT DEFAULT (datetime('now')))",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

/// Insert a session whose `updated_at` is exactly `minutes_ago` minutes in
/// the past, written the same way BigTiny writes it (`datetime('now', ...)`,
/// not a bound chrono value) so the fixture matches production shape.
async fn insert_session(pool: &SqlitePool, id: &str, status: &str, minutes_ago: i64) {
    sqlx::query(
        "INSERT INTO sessions (id, status, updated_at) \
         VALUES (?, ?, datetime('now', printf('-%d minutes', ?)))",
    )
    .bind(id)
    .bind(status)
    .bind(minutes_ago)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn fresh_idle_session_is_not_swept() {
    let pool = host_pool().await;
    insert_session(&pool, "fresh", "idle", 1).await;
    let ids = idle_session_ids(&pool, None, 15, 30).await.unwrap();
    assert!(ids.is_empty(), "a session idle for only 1 minute must not be swept at a 15-minute cutoff");
}

#[tokio::test]
async fn stale_idle_session_is_swept() {
    let pool = host_pool().await;
    insert_session(&pool, "stale", "idle", 20).await;
    let ids = idle_session_ids(&pool, None, 15, 30).await.unwrap();
    assert_eq!(ids, vec!["stale".to_string()]);
}

#[tokio::test]
async fn active_session_needs_the_longer_cutoff() {
    let pool = host_pool().await;
    // Active for 20 minutes: past the 15-minute idle cutoff but not the
    // 30-minute active (crashed-mid-turn) cutoff.
    insert_session(&pool, "still-working", "active", 20).await;
    let ids = idle_session_ids(&pool, None, 15, 30).await.unwrap();
    assert!(ids.is_empty(), "an active session under the active cutoff must not be swept");
}

#[tokio::test]
async fn stale_active_session_is_swept_as_crashed() {
    let pool = host_pool().await;
    insert_session(&pool, "crashed", "active", 45).await;
    let ids = idle_session_ids(&pool, None, 15, 30).await.unwrap();
    assert_eq!(ids, vec!["crashed".to_string()]);
}

// ---- the sweep itself (issue: the same three sessions forever) ----

/// A host db with the app column and messages, the shape `idle_sweep` reads.
async fn app_host_pool() -> SqlitePool {
    let options = SqliteConnectOptions::from_str("sqlite::memory:").unwrap();
    let pool = SqlitePool::connect_with(options).await.unwrap();
    sqlx::query(
        "CREATE TABLE sessions (id TEXT PRIMARY KEY, status TEXT DEFAULT 'idle', \
         updated_at TEXT DEFAULT (datetime('now')), app_id TEXT NOT NULL DEFAULT '')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE messages (session_id TEXT, role TEXT, content TEXT, \
         rowid INTEGER PRIMARY KEY AUTOINCREMENT)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

/// An idle session of `app_id`, last touched `minutes_ago`, with one user
/// message. Returns that message's rowid.
async fn insert_app_session(pool: &SqlitePool, id: &str, app_id: &str, minutes_ago: i64) -> i64 {
    sqlx::query(
        "INSERT INTO sessions (id, status, updated_at, app_id) \
         VALUES (?, 'idle', datetime('now', printf('-%d minutes', ?)), ?)",
    )
    .bind(id)
    .bind(minutes_ago)
    .bind(app_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO messages (session_id, role, content) VALUES (?, 'user', ?)")
        .bind(id)
        .bind(format!("I prefer terse code comments ({id})."))
        .execute(pool)
        .await
        .unwrap()
        .last_insert_rowid()
}

fn chat_one() -> std::sync::Arc<MockChat> {
    std::sync::Arc::new(MockChat {
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
    })
}

#[tokio::test]
async fn sweep_reaches_a_session_behind_three_finished_ones() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    let host = app_host_pool().await;
    // Three recent sessions, already learned through their last message.
    for (id, ago) in [("done-1", 20), ("done-2", 25), ("done-3", 30)] {
        let rowid = insert_app_session(&host, id, "kitty", ago).await;
        engine.db.advance_learned_rowid(id, rowid).await.unwrap();
    }
    // An older one nobody has learned from.
    let waiting = insert_app_session(&host, "waiting", "kitty", 60).await;

    let mut backoff = SweepBackoff::new();
    idle_sweep(&engine, &host, &chat_one(), Some("kitty"), &mut backoff).await.unwrap();

    assert_eq!(
        engine.db.last_learned_rowid("waiting").await.unwrap(),
        waiting,
        "finished sessions must not take the batch slots"
    );
}

#[tokio::test]
async fn sweep_never_learns_another_apps_session() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    let host = app_host_pool().await;
    insert_app_session(&host, "theirs", "notebook", 20).await;

    let mut backoff = SweepBackoff::new();
    idle_sweep(&engine, &host, &chat_one(), Some("kitty"), &mut backoff).await.unwrap();

    assert_eq!(engine.db.last_learned_rowid("theirs").await.unwrap(), 0);
    assert!(engine.db.list_beliefs(None).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_finished_session_is_not_reconsolidated_every_tick() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    let host = app_host_pool().await;
    let rowid = insert_app_session(&host, "done", "kitty", 20).await;
    engine.db.advance_learned_rowid("done", rowid).await.unwrap();

    let mut backoff = SweepBackoff::new();
    for _ in 0..3 {
        idle_sweep(&engine, &host, &chat_one(), Some("kitty"), &mut backoff).await.unwrap();
    }

    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE event = 'consolidate'")
            .fetch_one(engine.db.pool())
            .await
            .unwrap();
    assert_eq!(rows, 0, "nothing changed, so nothing should be audited");
}
