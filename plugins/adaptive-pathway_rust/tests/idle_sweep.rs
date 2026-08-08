//! Regression coverage for issue #3: `idle_session_ids` must actually
//! compare ages, not report every session as idle immediately.
//!
//! The `sessions` table here mirrors `bigtiny.db`'s real shape: `updated_at`
//! written via bare `CURRENT_TIMESTAMP`/`datetime('now')`, i.e. naive
//! `'YYYY-MM-DD HH:MM:SS'` text -- not RFC3339. A cutoff bound in from a
//! Rust `chrono::DateTime` (RFC3339, `T` separator) would never compare
//! correctly against that.

use adaptive_pathway::learn::host::idle_session_ids;
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
    let ids = idle_session_ids(&pool, 15, 30).await.unwrap();
    assert!(ids.is_empty(), "a session idle for only 1 minute must not be swept at a 15-minute cutoff");
}

#[tokio::test]
async fn stale_idle_session_is_swept() {
    let pool = host_pool().await;
    insert_session(&pool, "stale", "idle", 20).await;
    let ids = idle_session_ids(&pool, 15, 30).await.unwrap();
    assert_eq!(ids, vec!["stale".to_string()]);
}

#[tokio::test]
async fn active_session_needs_the_longer_cutoff() {
    let pool = host_pool().await;
    // Active for 20 minutes: past the 15-minute idle cutoff but not the
    // 30-minute active (crashed-mid-turn) cutoff.
    insert_session(&pool, "still-working", "active", 20).await;
    let ids = idle_session_ids(&pool, 15, 30).await.unwrap();
    assert!(ids.is_empty(), "an active session under the active cutoff must not be swept");
}

#[tokio::test]
async fn stale_active_session_is_swept_as_crashed() {
    let pool = host_pool().await;
    insert_session(&pool, "crashed", "active", 45).await;
    let ids = idle_session_ids(&pool, 15, 30).await.unwrap();
    assert_eq!(ids, vec!["crashed".to_string()]);
}
