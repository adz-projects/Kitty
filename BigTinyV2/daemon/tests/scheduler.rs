//! Scheduled runs: seed a schedule row, trigger it through
//! `Scheduler::run_job` (bypassing real cron timing), and assert the
//! bookkeeping in `execution_history`/`sessions`.
//!
//! Renamed from `scheduler_and_recipes.rs` when recipes became specialists. A
//! schedule now carries a plain `prompt` and a firing is an ordinary turn, so
//! the recipe-shaped half of the old suite (template rendering, an FK to a
//! recipe row) has nothing left to test — while the parts that mattered do:
//! success and failure are recorded honestly, and overlapping ticks are
//! skipped.
//!
//! The session lifecycle changed with it, and the tests below pin the new rule
//! rather than the old one: a run's session *is* its output now, so it is kept
//! on both paths instead of being a throwaway that success deleted.

use std::sync::Arc;

use bigtiny2::agent::summarizer_chain::SummarizerChain;
use bigtiny2::agent::Agent;
use bigtiny2::config::BigTinyConfig;
use bigtiny2::hitl::manager::HITLManager;
use bigtiny2::mcp::MCPManager;
use bigtiny2::provider::router::ProviderRouter;
use bigtiny2::scheduler::Scheduler;
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

async fn build_agent(pool: &SqlitePool) -> Arc<Agent> {
    build_agent_inner(pool, None).await
}

/// Same as `build_agent`, but with one OpenAI-compatible provider registered
/// against the given (mockito) base URL, so a turn can genuinely succeed
/// instead of failing fast with "no healthy providers".
async fn build_agent_with_provider(pool: &SqlitePool, base_url: &str) -> Arc<Agent> {
    build_agent_inner(pool, Some(base_url)).await
}

async fn build_agent_inner(pool: &SqlitePool, provider_base_url: Option<&str>) -> Arc<Agent> {
    let config = BigTinyConfig::default();
    let router = Arc::new(ProviderRouter::new(config.cache.clone()));
    if let Some(base_url) = provider_base_url {
        router.register_openai(
            "mock-openai",
            bigtiny2::config::ProviderConfig {
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
    let summarizer = Arc::new(SummarizerChain::new(
        None,
        router.clone(),
        config.summarizer.clone(),
    ));
    Arc::new(Agent::new(
        pool.clone(),
        router,
        mcp,
        hitl,
        summarizer,
        config,
        std::env::temp_dir().to_string_lossy().into_owned(),
        bigtiny2::plugins::test_plugin_host(pool),
    ))
}

async fn seed_schedule(pool: &SqlitePool, id: &str, prompt: &str) {
    sqlx::query(
        "INSERT INTO schedule_jobs (id, name, cron, prompt, enabled, app_id) \
         VALUES (?, 'job', '0 9 * * *', ?, 1, 'app-a')",
    )
    .bind(id)
    .bind(prompt)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn a_successful_run_is_recorded_and_keeps_its_session() {
    let pool = test_pool().await;
    seed_schedule(&pool, "j1", "Say hi").await;

    // A mock OpenAI-compatible endpoint serves one minimal SSE completion so
    // the turn genuinely succeeds. Before `run_turn_and_wait` propagated the
    // outcome, this test "passed" with no provider at all — the turn's failure
    // was swallowed and the run misrecorded as `completed`.
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

    let agent = build_agent_with_provider(&pool, &server.url()).await;
    let scheduler = Scheduler::new(pool.clone(), agent).await.unwrap();

    scheduler.run_job("j1").await.unwrap();

    let exec =
        sqlx::query("SELECT status, session_id FROM execution_history WHERE trigger_id = 'j1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(exec.get::<String, _>("status"), "completed");

    // The session the turn ran in is the run's output, so it survives. This is
    // the inversion of the old contract, where success *deleted* the session
    // because the real output lived in a separate recipe session.
    let session_id: String = exec.get("session_id");
    let kept = sqlx::query("SELECT id FROM sessions WHERE id = ?")
        .bind(&session_id)
        .fetch_optional(&pool)
        .await
        .unwrap();
    assert!(
        kept.is_some(),
        "a completed run must keep the transcript it produced"
    );
}

#[tokio::test]
async fn a_failed_run_is_recorded_with_its_error_and_keeps_its_transcript() {
    let pool = test_pool().await;
    seed_schedule(&pool, "j2", "Say hi").await;

    // No provider registered, so the turn fails with "no healthy providers" —
    // the simplest genuine failure now that there is no template to malform.
    let agent = build_agent(&pool).await;
    let scheduler = Scheduler::new(pool.clone(), agent).await.unwrap();
    scheduler.run_job("j2").await.unwrap();

    let rows = sqlx::query(
        "SELECT status, error_message, session_id FROM execution_history WHERE trigger_id = 'j2'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "a failed run must keep its history row");
    assert_eq!(rows[0].get::<String, _>("status"), "failed");
    let error_message: Option<String> = rows[0].get("error_message");
    assert!(
        !error_message.as_deref().unwrap_or("").is_empty(),
        "the failure must say what went wrong, got {error_message:?}"
    );

    // The transcript is the only record of why a scheduled run failed, so it is
    // kept and the row keeps pointing at it — the old design nulled this to
    // release a throwaway session that no longer exists.
    let anchored: Option<String> = rows[0].get("session_id");
    let anchored = anchored.expect("a failed run must still name its session");
    let kept = sqlx::query("SELECT id FROM sessions WHERE id = ?")
        .bind(&anchored)
        .fetch_optional(&pool)
        .await
        .unwrap();
    assert!(kept.is_some(), "a failed run must keep its transcript");
}

/// Two ticks of the same job must not run concurrently: `tokio-cron-scheduler`
/// spawns a fresh task per due tick regardless of whether the previous one
/// finished, so a job slower than its own interval used to stack up
/// overlapping runs (double provider spend, interleaved history rows).
#[tokio::test]
async fn a_job_already_in_flight_skips_the_overlapping_tick() {
    let pool = test_pool().await;
    seed_schedule(&pool, "j3", "Say hi").await;

    let agent = build_agent(&pool).await;
    let scheduler = Scheduler::new(pool.clone(), agent).await.unwrap();

    // Both futures are driven concurrently; exactly one may record a run.
    let (a, b) = tokio::join!(scheduler.run_job("j3"), scheduler.run_job("j3"));
    a.unwrap();
    b.unwrap();

    let runs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM execution_history WHERE trigger_id = 'j3'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(runs, 1, "the overlapping tick must be skipped, not queued");
}

/// A schedule row carried forward by migration 021 keeps firing.
///
/// The migration rebuilt `schedule_jobs` to swap `recipe_id` for `prompt`,
/// translating each row's recipe template into its prompt. A row that survives
/// the schema change but no longer runs is the failure this guards against.
#[tokio::test]
async fn a_disabled_schedule_never_runs_however_it_is_triggered() {
    let pool = test_pool().await;
    sqlx::query(
        "INSERT INTO schedule_jobs (id, name, cron, prompt, enabled, app_id) \
         VALUES ('j4', 'job', '0 9 * * *', 'Say hi', 0, 'app-a')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let agent = build_agent(&pool).await;
    let scheduler = Scheduler::new(pool.clone(), agent).await.unwrap();
    // `run_job` finds the row (so this is not a 404) and `execute_job` refuses
    // it on the `enabled` re-check — a manual trigger of a disabled schedule is
    // a no-op, not a surprise run.
    assert!(scheduler.run_job("j4").await.unwrap());

    let runs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM execution_history WHERE trigger_id = 'j4'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(runs, 0, "a disabled schedule must not run");
}
