//! Phase F verification: seed a recipe + schedule row, trigger it via
//! `Scheduler::run_job` (bypassing real cron timing), and assert the
//! success/failure bookkeeping in `execution_history`/`sessions`: success
//! marks `completed` and cleans up the temp session; failure (incl. a
//! provider-failed turn, now that `run_turn_and_wait` propagates the
//! outcome — 815bugs #91) marks the row `failed` with the error message and
//! keeps the temp session as the row's FK anchor (see
//! `src/scheduler/mod.rs::execute_job`).

use std::sync::Arc;

use bigtiny_rust::agent::summarizer_chain::SummarizerChain;
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
    build_engine_inner(pool, None).await
}

/// Same as `build_engine`, but with one OpenAI-compatible provider
/// registered against the given (mockito) base URL, so a turn can genuinely
/// succeed instead of failing fast with "no healthy providers".
async fn build_engine_with_provider(pool: &SqlitePool, base_url: &str) -> Arc<RecipeEngine> {
    build_engine_inner(pool, Some(base_url)).await
}

async fn build_engine_inner(pool: &SqlitePool, provider_base_url: Option<&str>) -> Arc<RecipeEngine> {
    let config = BigTinyConfig::default();
    let router = Arc::new(ProviderRouter::new(config.cache.clone()));
    if let Some(base_url) = provider_base_url {
        router.register_openai(
            "mock-openai",
            bigtiny_rust::config::ProviderConfig {
                base_url: base_url.to_string(),
                ..Default::default()
            },
        );
    }
    let mcp = Arc::new(MCPManager::new(pool.clone(), None));
    let hitl = Arc::new(tokio::sync::Mutex::new(HITLManager::new(
        pool.clone(),
        config.hitl.clone(),
    )));
    #[cfg(feature = "local-engine")]
    let summarizer = Arc::new(SummarizerChain::new(None, router.clone(), config.summarizer.clone()));
    #[cfg(not(feature = "local-engine"))]
    let summarizer = Arc::new(SummarizerChain::new(router.clone(), config.summarizer.clone()));
    let agent = Arc::new(Agent::new(
        pool.clone(),
        router,
        mcp.clone(),
        hitl,
        summarizer,
        config,
        std::env::temp_dir().to_string_lossy().into_owned(),
        None,
        None,
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

    // A mock OpenAI-compatible endpoint serves one minimal SSE completion,
    // so the agent turn genuinely succeeds. (Before 815bugs #91 this test
    // "passed" with NO provider at all — the turn's failure was swallowed
    // and the run misrecorded as `completed`.)
    let mut server = mockito::Server::new_async().await;
    let _mock = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hi!\"},\"finish_reason\":null}]}\n\n\
             data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1,\"total_tokens\":4}}\n\n\
             data: [DONE]\n\n",
        )
        .create_async()
        .await;

    let engine = build_engine_with_provider(&pool, &server.url()).await;
    let scheduler = Scheduler::new(pool.clone(), engine).await.unwrap();

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
async fn run_job_failure_cleans_up_temp_session_and_execution_history() {
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

    // Failed runs are now recorded, not erased (815bugs #91): the
    // `execution_history` row is marked `failed` with the error message, and
    // the `_job_*` temp session stays as the row's FK anchor. Previously a
    // failure deleted both rows — and a provider-failed turn was misrecorded
    // as `completed` — so failures were invisible in the history either way.
    let exec_rows = sqlx::query(
        "SELECT status, error_message FROM execution_history WHERE trigger_id = 'j2'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(exec_rows.len(), 1, "failed run must keep its history row");
    let status: String = exec_rows[0].get("status");
    assert_eq!(status, "failed");
    let error_message: Option<String> = exec_rows[0].get("error_message");
    assert!(
        error_message.as_deref().unwrap_or("").contains("template"),
        "error_message should capture the failure, got: {error_message:?}"
    );

    let temp_sessions = sqlx::query("SELECT id FROM sessions WHERE id LIKE '_job_%'")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        temp_sessions.len(),
        1,
        "temp `_job_*` session stays as the failed row's FK anchor"
    );
}
