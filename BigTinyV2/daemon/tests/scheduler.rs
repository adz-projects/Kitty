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
        bigtiny2::plugins::test_memorabilia_host(pool),
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

// ---------------------------------------------------------------------------
// Schedules v2: outcomes, events, run configuration, timer kinds
// ---------------------------------------------------------------------------

/// A mock provider that answers every completion with a short success.
async fn ok_provider() -> mockito::ServerGuard {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(
            "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Done.\"},\"finish_reason\":null}]}\n\n\
             data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1,\"total_tokens\":4}}\n\n\
             data: [DONE]\n\n",
        )
        .create_async()
        .await;
    server
}

async fn schedule_row(pool: &SqlitePool, id: &str) -> bigtiny2::storage::schedules::ScheduleRow {
    bigtiny2::storage::schedules::get_schedule(pool, id)
        .await
        .unwrap()
        .unwrap()
}

/// Wait (bounded) until the schedule row satisfies `check`, for runs started
/// on a timer task.
async fn wait_for_row(
    pool: &SqlitePool,
    id: &str,
    check: impl Fn(&bigtiny2::storage::schedules::ScheduleRow) -> bool,
) {
    for _ in 0..100 {
        if check(&schedule_row(pool, id).await) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("schedule {id} never reached the expected state");
}

/// A run's outcome lands on the schedule row itself, the owning app hears
/// `started` then `finished` on its event stream, and the run's session is
/// configured the way the schedule says (a short approval wait by default).
#[tokio::test]
async fn a_run_records_its_outcome_and_announces_it() {
    use bigtiny2::server::events::SSEEventType;

    let pool = test_pool().await;
    seed_schedule(&pool, "j5", "Say hi").await;
    let server = ok_provider().await;
    let agent = build_agent_with_provider(&pool, &server.url()).await;
    let mut events = agent.app_events().subscribe();
    let scheduler = Scheduler::new(pool.clone(), agent).await.unwrap();

    scheduler.run_job("j5").await.unwrap();

    let row = schedule_row(&pool, "j5").await;
    assert_eq!(row.last_status.as_deref(), Some("completed"));
    let session_id = row.last_session_id.clone().expect("the run's session is recorded");
    assert!(row.last_run_at.is_some());

    let meta: String = sqlx::query_scalar("SELECT metadata FROM sessions WHERE id = ?")
        .bind(&session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta).unwrap();
    assert_eq!(meta["hitl_timeout_secs"], 600);
    assert_eq!(meta["schedule_id"], "j5");

    let mut outcomes = Vec::new();
    while let Ok((app, ev)) = events.try_recv() {
        if ev.event_type == SSEEventType::ScheduleRun {
            assert_eq!(app, "app-a", "announced to the schedule's owner");
            assert_eq!(ev.schedule_id.as_deref(), Some("j5"));
            outcomes.push(ev.content.unwrap());
        }
    }
    assert_eq!(outcomes, ["started", "finished"]);
}

/// A provider pin and a system prompt on the schedule reach the run session.
#[tokio::test]
async fn a_schedule_pins_its_runs_provider_and_persona() {
    use bigtiny2::storage::schedules::ScheduleSpec;

    let pool = test_pool().await;
    let server = ok_provider().await;
    let agent = build_agent_with_provider(&pool, &server.url()).await;
    let mut scheduler = Scheduler::new(pool.clone(), agent).await.unwrap();
    let id = scheduler
        .add_schedule(
            "app-a",
            ScheduleSpec {
                name: "pinned".into(),
                prompt: "hi".into(),
                kind: "cron".into(),
                cron: "0 9 * * *".into(),
                provider_id: Some("mock-openai".into()),
                model: Some("m1".into()),
                system_prompt: Some("Be terse.".into()),
                cwd: Some("C:/work/task".into()),
                hitl_timeout_secs: 120,
                enabled: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    scheduler.run_job(&id).await.unwrap();

    let session_id = schedule_row(&pool, &id).await.last_session_id.unwrap();
    let meta: String = sqlx::query_scalar("SELECT metadata FROM sessions WHERE id = ?")
        .bind(&session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta).unwrap();
    assert_eq!(meta["provider"], "mock-openai");
    assert_eq!(meta["model"], "m1");
    assert_eq!(meta["persona_override"], "Be terse.");
    assert_eq!(meta["hitl_timeout_secs"], 120);
    assert_eq!(meta["cwd"], "C:/work/task");
    assert_eq!(meta["chat_dir"], "C:/work/task");
}

/// A `once` schedule whose time has already passed runs as soon as it is
/// registered, then disables itself.
#[tokio::test]
async fn a_once_schedule_that_is_already_due_runs_then_disables_itself() {
    use bigtiny2::storage::schedules::ScheduleSpec;

    let pool = test_pool().await;
    let server = ok_provider().await;
    let agent = build_agent_with_provider(&pool, &server.url()).await;
    let mut scheduler = Scheduler::new(pool.clone(), agent).await.unwrap();
    let past = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
    let id = scheduler
        .add_schedule(
            "app-a",
            ScheduleSpec {
                name: "once".into(),
                prompt: "hi".into(),
                kind: "once".into(),
                run_at: Some(past),
                hitl_timeout_secs: 600,
                enabled: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    wait_for_row(&pool, &id, |r| r.enabled == 0 && r.last_status.as_deref() == Some("completed")).await;
    assert!(schedule_row(&pool, &id).await.next_run_at.is_none());
}

/// An interval schedule that fell due while the daemon was down runs once on
/// start -- not once per missed interval -- and is rescheduled from now.
#[tokio::test]
async fn an_overdue_interval_schedule_catches_up_once_on_start() {
    use bigtiny2::storage::schedules::{self, ScheduleSpec};

    let pool = test_pool().await;
    let overdue = (chrono::Utc::now() - chrono::Duration::hours(3)).to_rfc3339();
    schedules::create_schedule_spec(
        &pool,
        "iv",
        "app-a",
        &ScheduleSpec {
            name: "hourly".into(),
            prompt: "hi".into(),
            kind: "interval".into(),
            interval_secs: Some(3600),
            next_run_at: Some(overdue),
            hitl_timeout_secs: 600,
            enabled: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let server = ok_provider().await;
    let agent = build_agent_with_provider(&pool, &server.url()).await;
    let mut scheduler = Scheduler::new(pool.clone(), agent).await.unwrap();
    scheduler.start().await.unwrap();

    // Completed, and its next run already moved into the future.
    wait_for_row(&pool, "iv", |r| {
        r.last_status.as_deref() == Some("completed")
            && r.next_run_at
                .as_deref()
                .and_then(|n| chrono::DateTime::parse_from_rfc3339(n).ok())
                .is_some_and(|n| n.with_timezone(&chrono::Utc) > chrono::Utc::now())
    })
    .await;
    let row = schedule_row(&pool, "iv").await;
    assert_eq!(row.enabled, 1, "an interval schedule stays enabled");
    let next = chrono::DateTime::parse_from_rfc3339(row.next_run_at.as_deref().unwrap()).unwrap();
    let ahead = (next.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds();
    assert!((3500..=3600).contains(&ahead), "rescheduled from now, got {ahead}s ahead");
    let runs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM execution_history WHERE trigger_id = 'iv'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(runs, 1, "one catch-up run, not one per missed interval");
    scheduler.stop().await;
}
