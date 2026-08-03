//! Phase F verification: seed a recipe + schedule row, trigger it via
//! `Scheduler::run_job` (bypassing real cron timing), and assert the
//! success/failure bookkeeping in `execution_history`/`sessions` matches
//! Python's `_execute_job` — including the deliberate asymmetry where a
//! failed run's temp session is marked `failed`, not deleted (see the
//! comment in `src/scheduler/mod.rs::execute_job`).

use std::sync::Arc;

use bigtiny_rust::agent::summarizer::SummarizerClient;
use bigtiny_rust::agent::Agent;
use bigtiny_rust::config::BigTinyConfig;
use bigtiny_rust::hitl::manager::HITLManager;
use bigtiny_rust::mcp::MCPManager;
use bigtiny_rust::provider::router::ProviderRouter;
use bigtiny_rust::recipes::engine::RecipeEngine;
use bigtiny_rust::scheduler::Scheduler;
use sqlx::{Row, SqlitePool};

async fn test_pool() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    pool
}

async fn build_engine(pool: &SqlitePool) -> Arc<RecipeEngine> {
    let config = BigTinyConfig::default();
    let router = Arc::new(ProviderRouter::new(config.cache.clone()));
    let mcp = Arc::new(MCPManager::new(pool.clone()));
    let hitl = Arc::new(tokio::sync::Mutex::new(HITLManager::new(
        pool.clone(),
        config.hitl.clone(),
    )));
    let summarizer = Arc::new(SummarizerClient::new(config.summarizer.clone()));
    let agent = Arc::new(Agent::new(
        pool.clone(),
        router,
        mcp.clone(),
        hitl,
        summarizer,
        config,
        std::env::temp_dir().to_string_lossy().into_owned(),
    ));
    Arc::new(RecipeEngine::new(
        pool.clone(),
        agent,
        mcp,
        std::env::temp_dir(),
    ))
}

#[tokio::test]
async fn run_job_success_completes_execution_and_deletes_temp_session() {
    let pool = test_pool().await;
    sqlx::query("INSERT INTO recipes (id, name, prompt_template, max_steps) VALUES ('r1', 'Test Recipe', 'Say hi', 30)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO schedule_jobs (id, name, cron, recipe_id, enabled) VALUES ('j1', 'job', '0 9 * * *', 'r1', 1)")
        .execute(&pool)
        .await
        .unwrap();

    let engine = build_engine(&pool).await;
    let scheduler = Scheduler::new(pool.clone(), engine).await.unwrap();

    // No providers registered, so the agent turn itself fails fast (no
    // healthy provider) — but the recipe engine still successfully creates
    // and returns a session id, which is all `execute_job`'s success path
    // requires to mark the execution `completed`.
    scheduler.run_job("j1").await.unwrap();

    let exec =
        sqlx::query("SELECT status, session_id FROM execution_history WHERE trigger_id = 'j1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let status: String = exec.get("status");
    let session_id: String = exec.get("session_id");
    assert_eq!(status, "completed");

    // The real recipe session should exist and be distinct from the temp
    // bookkeeping session.
    assert!(!session_id.starts_with("_job_"));
    let real_session = sqlx::query("SELECT id FROM sessions WHERE id = ?")
        .bind(&session_id)
        .fetch_optional(&pool)
        .await
        .unwrap();
    assert!(real_session.is_some());

    // Temp session was deleted on success.
    let temp_sessions = sqlx::query("SELECT id FROM sessions WHERE id LIKE '_job_%'")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(temp_sessions.is_empty());
}

#[tokio::test]
async fn run_job_failure_marks_execution_failed_and_keeps_temp_session_as_failed() {
    let pool = test_pool().await;
    // A deliberately malformed Jinja template drives `RecipeEngine::execute`
    // into its `RecipeError::Template` path — recipes can't reference a
    // nonexistent recipe id (schedule_jobs.recipe_id is FK-enforced, and
    // recipes can't be deleted out from under a referencing schedule_jobs
    // row either), so a template error is the cleanest FK-safe way to
    // exercise the failure branch.
    sqlx::query("INSERT INTO recipes (id, name, prompt_template, max_steps) VALUES ('r2', 'Broken Recipe', '{% if %}', 30)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO schedule_jobs (id, name, cron, recipe_id, enabled) VALUES ('j2', 'job', '0 9 * * *', 'r2', 1)")
        .execute(&pool)
        .await
        .unwrap();

    let engine = build_engine(&pool).await;
    let scheduler = Scheduler::new(pool.clone(), engine).await.unwrap();
    scheduler.run_job("j2").await.unwrap();

    let exec = sqlx::query(
        "SELECT status, error_message, session_id FROM execution_history WHERE trigger_id = 'j2'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let status: String = exec.get("status");
    let session_id: String = exec.get("session_id");
    assert_eq!(status, "failed");
    let error_message: Option<String> = exec.get("error_message");
    assert!(error_message.is_some());

    // The temp session was NOT deleted (would violate the FK from
    // execution_history.session_id) — it's marked failed instead.
    assert!(session_id.starts_with("_job_"));
    let temp_session = sqlx::query("SELECT status FROM sessions WHERE id = ?")
        .bind(&session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let temp_status: String = temp_session.get("status");
    assert_eq!(temp_status, "failed");
}
