//! The properties that make a delegated run safe to hand a model.
//!
//! Each of these is a rule that fails *quietly* if it regresses — a tool
//! restriction that only shaped a prompt, an unattended run that blocks for an
//! hour, an unbounded fan-out, a cancelled parent leaving children spending. So
//! they are pinned end to end, through a real agent turn against a mock
//! provider, rather than by unit-testing the guard in isolation.

use std::sync::Arc;

use bigtiny2::agent::orchestrator::{DelegateRun, Orchestrator, SpawnRefusal};
use bigtiny2::agent::summarizer_chain::SummarizerChain;
use bigtiny2::agent::Agent;
use bigtiny2::config::BigTinyConfig;
use bigtiny2::hitl::manager::HITLManager;
use bigtiny2::mcp::MCPManager;
use bigtiny2::provider::router::ProviderRouter;
use bigtiny2::server::events::{SSEEvent, SSEEventType};
use bigtiny2::storage::sessions;
use serde_json::json;
use sqlx::SqlitePool;
use tokio::sync::mpsc;

const APP: &str = "test-app";

async fn test_pool() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    bigtiny2::storage::apps::register_app(&pool, APP, "Test App", "k")
        .await
        .unwrap();
    pool
}

fn build_agent(pool: &SqlitePool, provider_base_url: Option<&str>) -> Arc<Agent> {
    build_agent_with(pool, provider_base_url, None)
}

/// An agent whose MCP manager carries `orchestrator`, the way `lib.rs` wires
/// the daemon — which is what lets the loop see a session's tickets.
fn build_agent_with(
    pool: &SqlitePool,
    provider_base_url: Option<&str>,
    orchestrator: Option<Arc<Orchestrator>>,
) -> Arc<Agent> {
    let config = BigTinyConfig::default();
    let router = Arc::new(ProviderRouter::new(config.cache.clone()));
    if let Some(base_url) = provider_base_url {
        router.register_openai(
            "mock",
            bigtiny2::config::ProviderConfig {
                base_url: base_url.to_string(),
                ..Default::default()
            },
        );
    }
    let mcp = Arc::new(MCPManager::new(pool.clone(), None));
    if let Some(o) = orchestrator {
        mcp.attach_orchestrator(o);
    }
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

async fn seed_session(pool: &SqlitePool, id: &str, metadata: serde_json::Value) {
    sessions::create_session_for_app(pool, id, id, APP)
        .await
        .unwrap();
    sessions::update_session_config(pool, id, &metadata.to_string())
        .await
        .unwrap();
}

/// One SSE completion that calls `tool` once, then one that stops. Two mocks
/// rather than one, each expecting a single hit, so the turn gets a tool call
/// on its first step and a final answer on its second instead of looping.
async fn mock_tool_then_stop(server: &mut mockito::ServerGuard, tool: &str) {
    let call = format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"tool_calls\":[{{\"index\":0,\"id\":\"c1\",\"type\":\"function\",\"function\":{{\"name\":\"{tool}\",\"arguments\":\"{{}}\"}}}}]}},\"finish_reason\":null}}]}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
         data: [DONE]\n\n"
    );
    server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(call)
        .expect(1)
        .create_async()
        .await;
    server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"done\"},\"finish_reason\":null}]}\n\n\
             data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n",
        )
        .expect(1)
        .create_async()
        .await;
}

/// Drive a turn to completion and return everything it emitted.
async fn run_and_collect(agent: &Arc<Agent>, session_id: &str) -> Vec<SSEEvent> {
    let (tx, mut rx) = mpsc::unbounded_channel::<SSEEvent>();
    agent
        .run_turn(session_id.to_string(), "go".into(), None, None, tx)
        .expect("turn should start");
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    events
}

fn tool_results(events: &[SSEEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.event_type == SSEEventType::ToolFinish)
        .filter_map(|e| e.tool_result.clone())
        .collect()
}

/// The allow-list is a boundary, not a prompt hint.
///
/// Filtering the advertised tool set is not enough: a model can call a tool it
/// was never offered, from a stale prompt prefix or plain invention. Here it
/// names one explicitly, and the dispatch site must refuse it.
#[tokio::test]
async fn a_tool_outside_the_allow_list_is_refused_even_when_the_model_names_it() {
    let pool = test_pool().await;
    let mut server = mockito::Server::new_async().await;
    mock_tool_then_stop(&mut server, "lean_file_write").await;
    let agent = build_agent(&pool, Some(&server.url()));

    seed_session(
        &pool,
        "s-allow",
        json!({"provider": "mock", "tool_allow": ["lean_file_read"]}),
    )
    .await;

    let events = run_and_collect(&agent, "s-allow").await;
    let results = tool_results(&events);
    assert_eq!(results.len(), 1, "the call must produce a result, got {results:?}");
    assert!(
        results[0].contains("not available to this run"),
        "expected a refusal naming the restriction, got: {}",
        results[0]
    );
    assert!(
        results[0].contains("lean_file_read"),
        "the refusal should say what IS available, got: {}",
        results[0]
    );
}

/// An empty allow-list means no tools, not unrestricted.
///
/// Reading it the other way would hand the most restricted definition the
/// widest surface — the exact inversion that makes a misconfiguration
/// dangerous rather than merely useless.
#[tokio::test]
async fn an_empty_allow_list_permits_nothing() {
    let pool = test_pool().await;
    let mut server = mockito::Server::new_async().await;
    mock_tool_then_stop(&mut server, "lean_file_read").await;
    let agent = build_agent(&pool, Some(&server.url()));

    seed_session(
        &pool,
        "s-empty",
        json!({"provider": "mock", "tool_allow": []}),
    )
    .await;

    let results = tool_results(&run_and_collect(&agent, "s-empty").await);
    assert_eq!(results.len(), 1);
    assert!(
        results[0].contains("not available to this run"),
        "got: {}",
        results[0]
    );
}

/// A session with no allow-list keeps every tool its app can see.
///
/// The companion to the two above: the restriction must not leak into ordinary
/// chat, where the absence of a list means "everything", not "nothing".
#[tokio::test]
async fn a_session_without_an_allow_list_is_unrestricted() {
    let pool = test_pool().await;
    let mut server = mockito::Server::new_async().await;
    mock_tool_then_stop(&mut server, "some_unregistered_tool").await;
    let agent = build_agent(&pool, Some(&server.url()));

    // `auto_reject` as well, and not incidentally: under the daemon default of
    // `always_ask` this unrestricted call reaches HITL and waits out the full
    // hour of `HITL_APPROVAL_TIMEOUT` with nobody to answer it. That is the
    // failure the policy exists to prevent, and it is what this test hit before
    // the policy was set here.
    seed_session(
        &pool,
        "s-open",
        json!({"provider": "mock", "hitl_policy": "auto_reject"}),
    )
    .await;

    let results = tool_results(&run_and_collect(&agent, "s-open").await);
    assert_eq!(results.len(), 1);
    assert!(
        !results[0].contains("not available to this run"),
        "an unrestricted session must not be refused by the allow-list guard. Got: {}",
        results[0]
    );
}

/// An unattended run proceeds on a tool its own definition allows.
///
/// This is the whole feature working or not working. The shipped default HITL
/// policy is `always_ask` and a delegate has no approver, so before this every
/// specialist that used a tool was refused on its first call, on every run: the
/// researcher could not search, the summarizer could not read. `tool_allow` is
/// the decision the approval prompt would have asked for, made in advance when
/// the specialist was written, so an undecided call on a listed tool proceeds.
#[tokio::test]
async fn an_unattended_run_proceeds_on_a_tool_its_definition_allows() {
    let pool = test_pool().await;
    let mut server = mockito::Server::new_async().await;
    mock_tool_then_stop(&mut server, "some_tool_needing_approval").await;
    let agent = build_agent(&pool, Some(&server.url()));

    seed_session(
        &pool,
        "s-hitl-allow",
        json!({
            "provider": "mock",
            "hitl_policy": "auto_reject",
            "tool_allow": ["some_tool_needing_approval"],
        }),
    )
    .await;

    let events = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_and_collect(&agent, "s-hitl-allow"),
    )
    .await
    .expect("an unattended run must not block on approval");

    let results = tool_results(&events);
    assert_eq!(results.len(), 1);
    // No MCP server is registered in this harness, so the call lands on
    // "Unknown tool" at dispatch. That *is* the assertion: reaching dispatch at
    // all means HITL let it through. What must not appear is the refusal.
    assert!(
        !results[0].contains("unattended"),
        "an allow-listed tool must not be refused for want of an approver. Got: {}",
        results[0]
    );
    assert!(
        !events
            .iter()
            .any(|e| e.event_type == SSEEventType::HitlPause),
        "no approval may be requested when there is nobody to answer it"
    );
}

/// ...but a rule the user stored still decides, and refuses immediately.
///
/// The pre-authorization above covers exactly one case: nobody has decided
/// anything about this tool. A stored `reject` rule is a decision, and an
/// unattended run must honour it rather than walk past it because its
/// definition happened to name the tool. `from_default_policy` is what keeps
/// those two apart; `hitl::manager`'s own tests pin the flag for every other
/// classification, including the containment escalation and a rule too damaged
/// to apply.
///
/// The refusal must also be legible enough for the delegate to report, which is
/// what keeps a blocked run from returning a quietly thinner answer than its
/// caller believes. And it must be immediate: `always_ask`'s wait is bounded at
/// an hour, which is not a hang but is indistinguishable from one to a caller,
/// and it is an hour *per tool call*.
#[tokio::test]
async fn an_unattended_run_refuses_a_tool_a_stored_rule_rejects() {
    let pool = test_pool().await;
    let mut server = mockito::Server::new_async().await;
    mock_tool_then_stop(&mut server, "some_tool_needing_approval").await;
    let agent = build_agent(&pool, Some(&server.url()));

    bigtiny2::storage::hitl_rules::upsert_rule(
        &pool,
        APP,
        "some_tool_needing_approval",
        None,
        "reject",
    )
    .await
    .unwrap();

    seed_session(
        &pool,
        "s-hitl",
        json!({
            "provider": "mock",
            "hitl_policy": "auto_reject",
            "tool_allow": ["some_tool_needing_approval"],
        }),
    )
    .await;

    // Ten seconds is three orders of magnitude below `HITL_APPROVAL_TIMEOUT`,
    // so this fails on a regression rather than passing slowly.
    let events = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_and_collect(&agent, "s-hitl"),
    )
    .await
    .expect("an unattended run must not block on approval");

    let results = tool_results(&events);
    assert_eq!(results.len(), 1);
    assert!(
        results[0].contains("denied") && results[0].contains("some_tool_needing_approval"),
        "the refusal must name the tool so the delegate can report it, got: {}",
        results[0]
    );
    assert!(
        !events
            .iter()
            .any(|e| e.event_type == SSEEventType::HitlPause),
        "no approval may be requested when there is nobody to answer it"
    );
}

/// Delegates may not delegate.
///
/// Structural, so that a user-defined specialist listing the spawn tool cannot
/// undo it: the check is on the parent's own parentage, not on any allow-list.
#[tokio::test]
async fn a_delegate_cannot_start_another_delegate() {
    let pool = test_pool().await;
    let agent = build_agent(&pool, None);
    let orchestrator = Arc::new(Orchestrator::new(pool.clone(), 3, 0.25, vec![], 300));
    orchestrator.attach(&agent);

    sessions::create_session_for_app(&pool, "root", "Root", APP)
        .await
        .unwrap();
    sessions::create_session_for_app(&pool, "child", "Child", APP)
        .await
        .unwrap();
    sessions::set_parent(&pool, "child", "root", APP)
        .await
        .unwrap();

    let run = DelegateRun {
        name: "researcher".into(),
        parent_session_id: "child".into(),
        prompt: "go".into(),
        system_prompt: None,
        provider: None,
        model: None,
        tool_allow: vec![],
        response_schema: None,
        max_steps: 5,
        reasoning_cap: None,
    };
    match orchestrator.run(run).await {
        Err(SpawnRefusal::TooDeep) => {}
        other => panic!("a delegate must not be able to delegate, got {other:?}"),
    }

    // And nothing was created on the way to refusing.
    let grandchildren = sessions::children_of(&pool, "child", APP).await.unwrap();
    assert!(grandchildren.is_empty());
}

/// A delegate inherits its parent's filesystem grants, and nothing more.
///
/// Without this the child's session is brand new, so `allowed_dirs_for_session`
/// finds no `chat_dir`, no `cwd`, no `working_dirs` and no `attached_paths`, and
/// every specialist whose job is reading the user's files is denied on its first
/// read — silently, because `hitl_policy: auto_reject` turns that denial into a
/// refusal rather than a prompt. The delegate then returns a confident report
/// based on nothing.
#[tokio::test]
async fn a_delegate_inherits_the_parents_filesystem_grants() {
    let pool = test_pool().await;
    let agent = build_agent(&pool, None);
    let orchestrator = Arc::new(Orchestrator::new(pool.clone(), 3, 0.25, vec![], 300));
    orchestrator.attach(&agent);

    seed_session(
        &pool,
        "granted",
        json!({
            "chat_dir": "/home/user/chat",
            "cwd": "/home/user/project",
            "working_dirs": ["/home/user/project", "/home/user/other"],
            "attached_paths": ["/home/user/docs/report.pdf"],
        }),
    )
    .await;

    // The run fails (no provider), which is fine: the child session and its
    // metadata are written before the turn starts, and that is what is under
    // test.
    let _ = orchestrator
        .run(DelegateRun {
            name: "locator".into(),
            parent_session_id: "granted".into(),
            prompt: "find it".into(),
            system_prompt: None,
            provider: None,
            model: None,
            tool_allow: vec!["lean_shell_ro".into()],
            response_schema: None,
            max_steps: 5,
            reasoning_cap: None,
        })
        .await;

    let children = sessions::children_of(&pool, "granted", APP).await.unwrap();
    assert_eq!(children.len(), 1);
    let child = sessions::get_session(&pool, &children[0])
        .await
        .unwrap()
        .expect("child session");
    let meta: serde_json::Value =
        serde_json::from_str(child.metadata.as_deref().unwrap_or("{}")).unwrap();

    assert_eq!(meta["cwd"], "/home/user/project");
    assert_eq!(meta["chat_dir"], "/home/user/chat");
    assert_eq!(meta["working_dirs"][1], "/home/user/other");
    assert_eq!(meta["attached_paths"][0], "/home/user/docs/report.pdf");

    // ...and nothing more: the delegate must not be able to reach anywhere its
    // parent could not.
    let dirs = bigtiny2::agent::sandbox::allowed_dirs_for_session(&meta, "/data");
    assert!(!bigtiny2::agent::sandbox::path_within_any(
        &dirs,
        "/home/user/somewhere-else/secret.txt"
    ));
}

/// A session with no owner cannot be a parent.
///
/// An ownerless child is unreachable by every scoped accessor in
/// `storage::sessions`, so the run would produce a transcript nobody could read
/// and a bill nobody could attribute.
#[tokio::test]
async fn a_delegate_of_an_unowned_session_is_refused() {
    let pool = test_pool().await;
    let agent = build_agent(&pool, None);
    let orchestrator = Arc::new(Orchestrator::new(pool.clone(), 3, 0.25, vec![], 300));
    orchestrator.attach(&agent);

    let run = DelegateRun {
        name: "researcher".into(),
        parent_session_id: "does-not-exist".into(),
        prompt: "go".into(),
        system_prompt: None,
        provider: None,
        model: None,
        tool_allow: vec![],
        response_schema: None,
        max_steps: 5,
        reasoning_cap: None,
    };
    assert!(matches!(
        orchestrator.run(run).await,
        Err(SpawnRefusal::NoOwner)
    ));
}

/// The denylist survives to the step that would otherwise bypass it.
///
/// The last thing `choose_host` tries before refusing is the parent's own
/// provider — and the parent's model is exactly the expensive one a denylist
/// exists to keep delegates off. Checked end to end here, not just in the
/// picker's unit tests, because the wiring is where it would be lost.
#[tokio::test]
async fn a_denylisted_model_is_refused_rather_than_run_as_the_fallback() {
    let pool = test_pool().await;
    let agent = build_agent(&pool, Some("http://127.0.0.1:9"));
    let router = Arc::new(ProviderRouter::new(BigTinyConfig::default().cache));
    router.register_openai(
        "mock",
        bigtiny2::config::ProviderConfig {
            base_url: "http://127.0.0.1:9".into(),
            model: "claude-fable-5-1".into(),
            ..Default::default()
        },
    );
    let orchestrator = Arc::new(Orchestrator::new(
        pool.clone(),
        3,
        0.25,
        vec!["claude-fable-*".to_string()],
        300,
    ));
    orchestrator.attach(&agent);
    orchestrator.attach_router(router);

    sessions::create_session_for_app(&pool, "parent", "Parent", APP)
        .await
        .unwrap();

    match orchestrator
        .run(DelegateRun {
            name: "researcher".into(),
            parent_session_id: "parent".into(),
            prompt: "go".into(),
            system_prompt: None,
            provider: None,
            model: None,
            tool_allow: vec![],
            response_schema: None,
            max_steps: 5,
            reasoning_cap: None,
        })
        .await
    {
        Err(SpawnRefusal::NoHost(why)) => {
            assert!(why.contains("denylist"), "must name the cause: {why}");
        }
        other => panic!("the denylist was bypassed: {other:?}"),
    }

    // And nothing was created on the way to refusing.
    let children = sessions::children_of(&pool, "parent", APP).await.unwrap();
    assert!(children.is_empty());
}

/// A delegate that never finishes must not hang the turn that called it.
///
/// `call_specialist` blocks the parent's tool call and holds one of a small
/// number of permits, and nothing else in the turn machinery bounds total
/// duration — the SSE idle timeout only bounds gaps between bytes, so a
/// steadily-streaming run is unbounded without this.
#[tokio::test]
async fn a_delegate_that_runs_too_long_is_stopped() {
    let pool = test_pool().await;

    // A provider that accepts the request and then never says anything more.
    let mut server = mockito::Server::new_async().await;
    let _stalled = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_chunked_body(|_w| {
            std::thread::sleep(std::time::Duration::from_secs(30));
            Ok(())
        })
        .create_async()
        .await;

    let agent = build_agent(&pool, Some(&server.url()));
    let router = Arc::new(ProviderRouter::new(BigTinyConfig::default().cache));
    router.register_openai(
        "mock",
        bigtiny2::config::ProviderConfig {
            base_url: server.url(),
            ..Default::default()
        },
    );
    // One second, so a regression fails fast rather than passing slowly.
    let orchestrator = Arc::new(Orchestrator::new(pool.clone(), 3, 0.25, vec![], 1));
    orchestrator.attach(&agent);
    orchestrator.attach_router(router);

    sessions::create_session_for_app(&pool, "parent", "Parent", APP)
        .await
        .unwrap();

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        orchestrator.run(DelegateRun {
            name: "researcher".into(),
            parent_session_id: "parent".into(),
            prompt: "go".into(),
            system_prompt: None,
            provider: None,
            model: None,
            tool_allow: vec![],
            response_schema: None,
            max_steps: 5,
            reasoning_cap: None,
        }),
    )
    .await
    .expect("the orchestrator itself must not hang");

    match outcome {
        Ok(Err(why)) => assert!(
            why.contains("longer than") && why.contains("stopped"),
            "the caller must be told why: {why}"
        ),
        other => panic!("expected a timeout result, got {other:?}"),
    }
    assert!(
        !agent.has_active_turns(),
        "a timed-out delegate must be cancelled, not merely abandoned"
    );
}

/// Concurrency is bounded by agents in flight, not requests.
///
/// The provider queue already bounds requests, and a delegate holds its slot
/// across the stretches where it is executing tools and sending nothing — so
/// without a separate cap, one turn that delegates ten times commits to ten
/// full agent runs before anyone sees the bill.
#[tokio::test]
async fn concurrent_delegates_are_capped() {
    let pool = test_pool().await;
    let agent = build_agent(&pool, None);
    let orchestrator = Arc::new(Orchestrator::new(pool.clone(), 2, 0.25, vec![], 300));
    orchestrator.attach(&agent);
    assert_eq!(orchestrator.limit(), 2);

    for i in 0..6 {
        sessions::create_session_for_app(&pool, &format!("p{i}"), "Parent", APP)
            .await
            .unwrap();
    }

    // Every run fails (no provider), which is fine: what is under test is that
    // six concurrent starts all complete rather than deadlocking on a permit,
    // and that each one produced exactly one child.
    let mut handles = Vec::new();
    for i in 0..6 {
        let orchestrator = orchestrator.clone();
        handles.push(tokio::spawn(async move {
            orchestrator
                .run(DelegateRun {
                    name: "researcher".into(),
                    parent_session_id: format!("p{i}"),
                    prompt: "go".into(),
                    system_prompt: None,
                    provider: None,
                    model: None,
                    tool_allow: vec![],
                    response_schema: None,
                    max_steps: 5,
                    reasoning_cap: None,
                })
                .await
                .map_err(|e| e.to_string())
        }));
    }
    for h in handles {
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), h)
            .await
            .expect("a capped delegate must not deadlock waiting for a permit")
            .unwrap();
        assert!(outcome.is_ok(), "spawn should not be refused: {outcome:?}");
    }

    let children: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sessions WHERE parent_session_id IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(children, 6, "every delegate should have run exactly once");
}

/// A fan-out is N delegates, and one failure is that element's alone.
///
/// The batch semantics are the whole point: a caller handed an error learns
/// nothing about the nine documents that worked, and re-running to find out is
/// exactly the expense the fan-out existed to avoid.
#[tokio::test]
async fn a_fan_out_runs_one_delegate_per_ref_and_survives_partial_failure() {
    let pool = test_pool().await;
    let agent = build_agent(&pool, None);
    let orchestrator = Arc::new(Orchestrator::new(pool.clone(), 3, 0.25, vec![], 300));
    orchestrator.attach(&agent);

    sessions::create_session_for_app(&pool, "parent", "Parent", APP)
        .await
        .unwrap();

    // No provider, so every child's turn fails — which is fine here: what is
    // under test is that three refs produce three independent runs with three
    // independent outcomes, not that any of them succeed.
    let specs: Vec<DelegateRun> = ["a.pdf", "b.pdf", "c.pdf"]
        .iter()
        .map(|r| DelegateRun {
            name: "extractor".into(),
            parent_session_id: "parent".into(),
            prompt: format!("extract from {r}"),
            system_prompt: None,
            provider: None,
            model: None,
            tool_allow: vec![],
            response_schema: None,
            max_steps: 5,
            reasoning_cap: None,
        })
        .collect();

    let outcomes = orchestrator.run_many(specs).await;
    assert_eq!(outcomes.len(), 3);
    for o in &outcomes {
        // Each element carries its own result rather than the batch collapsing
        // to one.
        assert!(matches!(o, Ok(Err(_))), "expected a per-element result: {o:?}");
    }

    let children = sessions::children_of(&pool, "parent", APP).await.unwrap();
    assert_eq!(children.len(), 3, "one delegate session per ref");
}

/// The two built-ins whose work is genuinely per-document declare the split;
/// the three whose work is *across* a corpus do not. Splitting a locator per
/// ref would give each delegate a view too narrow to answer with.
#[tokio::test]
async fn only_the_per_document_builtins_declare_a_fan_out() {
    let by_name: std::collections::HashMap<String, Option<String>> =
        bigtiny2::specialists::registry::all()
            .into_iter()
            .map(|s| (s.name, s.fan_out))
            .collect();

    assert_eq!(by_name["extractor"].as_deref(), Some("per_ref"));
    assert_eq!(by_name["summarizer"].as_deref(), Some("per_ref"));
    assert_eq!(by_name["locator"], None);
    assert_eq!(by_name["researcher"], None);
    assert_eq!(by_name["analyst"], None);
}

/// Cancelling a parent stops the work it caused.
///
/// A delegate exists to answer its parent's turn, so a cancelled parent leaves
/// its children with nobody to return to — and without propagation they keep
/// running to completion, spending on an answer no one will read.
#[tokio::test]
async fn cancelling_a_parent_cancels_its_delegates() {
    let pool = test_pool().await;

    // A provider that never responds, so the child's turn is genuinely still
    // in flight when the parent is cancelled.
    let mut server = mockito::Server::new_async().await;
    let _stalled = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_chunked_body(|_w| {
            std::thread::sleep(std::time::Duration::from_secs(60));
            Ok(())
        })
        .create_async()
        .await;
    let agent = build_agent(&pool, Some(&server.url()));

    sessions::create_session_for_app(&pool, "parent", "Parent", APP)
        .await
        .unwrap();
    seed_session(&pool, "kid", json!({"provider": "mock"})).await;
    sessions::set_parent(&pool, "kid", "parent", APP)
        .await
        .unwrap();

    let (tx, _rx) = mpsc::unbounded_channel::<SSEEvent>();
    agent
        .run_turn("kid".into(), "go".into(), None, None, tx)
        .unwrap();

    // Wait for the child's turn to actually be registered before cancelling —
    // cancelling before it starts would pass for the wrong reason.
    for _ in 0..100 {
        if agent.has_active_turns() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(agent.has_active_turns(), "the delegate turn never started");

    agent.cancel("parent").await;

    assert!(
        !agent.has_active_turns(),
        "cancelling the parent must stop the delegate it started"
    );
}

// --- Background specialists: tickets and the end-of-turn collection ---------

/// One streamed completion that says `text` and stops.
async fn mock_answer(server: &mut mockito::ServerGuard, text: &str) {
    server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"{text}\"}},\"finish_reason\":null}}]}}\n\n\
             data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
             data: [DONE]\n\n"
        ))
        .expect(1)
        .create_async()
        .await;
}

async fn ticket_agent(pool: &SqlitePool, url: &str) -> (Arc<Agent>, Arc<Orchestrator>) {
    let orchestrator = Arc::new(Orchestrator::new(pool.clone(), 3, 0.25, vec![], 300));
    let agent = build_agent_with(pool, Some(url), Some(orchestrator.clone()));
    orchestrator.attach(&agent);
    (agent, orchestrator)
}

/// A report that takes a moment, so the turn genuinely has to wait for it.
async fn slow_report(text: &'static str) -> String {
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    json!({"ok": true, "specialist": "researcher", "result": text}).to_string()
}

/// The rule the whole feature rests on: a turn cannot end on an answer written
/// before every specialist reported.
///
/// The model answers straight away with a ticket outstanding. That answer must
/// not end the turn; the loop collects the report itself, recorded as an
/// `await_specialists` call on the same assistant message as the early text,
/// and the model answers again with the report in context.
#[tokio::test]
async fn an_answer_with_a_ticket_outstanding_is_not_the_end_of_the_turn() {
    let pool = test_pool().await;
    let mut server = mockito::Server::new_async().await;
    mock_answer(&mut server, "draft").await;
    mock_answer(&mut server, "final").await;
    let (agent, orchestrator) = ticket_agent(&pool, &server.url()).await;
    seed_session(&pool, "parent", json!({"provider": "mock"})).await;

    let ticket = orchestrator.start_ticket("parent", "researcher", slow_report("the finding"));
    let events = run_and_collect(&agent, "parent").await;

    assert!(
        orchestrator.uncollected("parent").is_empty(),
        "the turn ended with {ticket} still outstanding"
    );
    let start = events
        .iter()
        .find(|e| {
            e.event_type == SSEEventType::ToolStart
                && e.tool_name.as_deref() == Some("await_specialists")
        })
        .expect("the loop must collect the outstanding report");
    assert_eq!(start.tool_args.as_ref().unwrap()["auto"], true);
    assert!(
        tool_results(&events).iter().any(|r| r.contains("the finding")),
        "the report must reach the model as a tool result"
    );

    let rows = bigtiny2::storage::messages::get_messages_by_session(&pool, "parent")
        .await
        .unwrap();
    let shape: Vec<(String, Option<String>)> = rows
        .iter()
        .map(|r| (r.role.clone(), r.content.clone()))
        .collect();
    let collecting = rows
        .iter()
        .position(|r| {
            r.role == "assistant"
                && r.tool_calls
                    .as_deref()
                    .is_some_and(|t| t.contains("await_specialists"))
        })
        .unwrap_or_else(|| panic!("no collection call persisted: {shape:?}"));
    assert_eq!(
        rows[collecting].content.as_deref(),
        Some("draft"),
        "the early answer travels on the collection call, not as its own turn"
    );
    assert_eq!(rows[collecting + 1].role, "tool");
    let last = rows.last().unwrap();
    assert_eq!(
        (last.role.as_str(), last.content.as_deref()),
        ("assistant", Some("final")),
        "the turn must end on an answer written after the report: {shape:?}"
    );
}

/// A model that collects its own tickets gets no synthetic call.
#[tokio::test]
async fn a_turn_with_nothing_outstanding_ends_on_its_first_answer() {
    let pool = test_pool().await;
    let mut server = mockito::Server::new_async().await;
    mock_answer(&mut server, "only").await;
    let (agent, orchestrator) = ticket_agent(&pool, &server.url()).await;
    seed_session(&pool, "parent", json!({"provider": "mock"})).await;

    let ticket = orchestrator.start_ticket("parent", "researcher", slow_report("x"));
    orchestrator
        .collect(
            "parent",
            Some(std::slice::from_ref(&ticket)),
            bigtiny2::agent::orchestrator::TicketWait::All,
        )
        .await
        .unwrap();

    let events = run_and_collect(&agent, "parent").await;
    assert!(!events.iter().any(|e| e.tool_name.as_deref() == Some("await_specialists")));
}

/// Running out of steps with a ticket outstanding earns one short extension —
/// enough to collect and answer — rather than ending on work already paid for.
#[tokio::test]
async fn the_step_limit_extends_once_to_collect_outstanding_reports() {
    let pool = test_pool().await;
    let mut server = mockito::Server::new_async().await;
    // Step 0 spends the whole budget on a tool call; the extension then covers
    // the early answer, the collection, and the real answer.
    mock_tool_then_stop(&mut server, "some_unregistered_tool").await;
    mock_answer(&mut server, "final").await;
    let (agent, orchestrator) = ticket_agent(&pool, &server.url()).await;
    // `auto_reject`, or the placeholder tool sits waiting for a human approval.
    seed_session(
        &pool,
        "parent",
        json!({"provider": "mock", "max_steps": 1, "hitl_policy": "auto_reject"}),
    )
    .await;

    orchestrator.start_ticket("parent", "researcher", slow_report("late finding"));
    let events = run_and_collect(&agent, "parent").await;

    assert!(orchestrator.uncollected("parent").is_empty());
    assert!(tool_results(&events).iter().any(|r| r.contains("late finding")));
    let rows = bigtiny2::storage::messages::get_messages_by_session(&pool, "parent")
        .await
        .unwrap();
    assert_eq!(rows.last().unwrap().content.as_deref(), Some("final"));
}

/// Cancelling a turn forgets its tickets and stops what they were running.
#[tokio::test]
async fn cancelling_a_parent_abandons_its_tickets() {
    let pool = test_pool().await;
    let server = mockito::Server::new_async().await;
    let (agent, orchestrator) = ticket_agent(&pool, &server.url()).await;
    sessions::create_session_for_app(&pool, "parent", "Parent", APP)
        .await
        .unwrap();

    orchestrator.start_ticket("parent", "researcher", async {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        String::new()
    });
    agent.cancel("parent").await;
    assert!(orchestrator.uncollected("parent").is_empty());
}

/// The point of the whole ticket design: a report reaches the model *during*
/// the turn, not at the end of it.
///
/// The model spends step 0 on a tool call while a delegate finishes underneath
/// it. The loop hands the report over the moment that step's results land, so
/// by the time the model writes anything it has already read it — and the
/// end-of-turn collection, which is a backstop for reports that arrive too
/// late, has nothing left to do. The `wait: "none"` on the call is what
/// distinguishes the two: nothing blocked to produce this.
#[tokio::test]
async fn a_report_that_finishes_mid_turn_reaches_the_model_on_its_next_step() {
    let pool = test_pool().await;
    let mut server = mockito::Server::new_async().await;
    // Two responses: a tool call, then an answer. No third is mocked, which is
    // itself part of the assertion — the report arrives during the tool step,
    // so the model never has to be asked to write its answer a second time.
    mock_tool_then_stop(&mut server, "some_unregistered_tool").await;
    let (agent, orchestrator) = ticket_agent(&pool, &server.url()).await;
    seed_session(
        &pool,
        "parent",
        json!({"provider": "mock", "hitl_policy": "auto_reject"}),
    )
    .await;

    // Ready almost at once, so it has certainly finished by the time step 0's
    // tool result is appended.
    orchestrator.start_ticket("parent", "researcher", async {
        json!({"ok": true, "result": "the early finding"}).to_string()
    });
    let events = run_and_collect(&agent, "parent").await;

    assert!(orchestrator.uncollected("parent").is_empty());
    let collections: Vec<&SSEEvent> = events
        .iter()
        .filter(|e| {
            e.event_type == SSEEventType::ToolStart
                && e.tool_name.as_deref() == Some("await_specialists")
        })
        .collect();
    assert_eq!(
        collections.len(),
        1,
        "the mid-turn hand-over should have left the end-of-turn backstop nothing to collect"
    );
    let args = collections[0].tool_args.as_ref().unwrap();
    assert_eq!(args["wait"], "none", "nothing should have blocked on this");
    assert_eq!(args["auto"], true);
    assert!(tool_results(&events)
        .iter()
        .any(|r| r.contains("the early finding")));

    let rows = bigtiny2::storage::messages::get_messages_by_session(&pool, "parent")
        .await
        .unwrap();
    let collecting = rows
        .iter()
        .position(|r| {
            r.role == "assistant"
                && r.tool_calls
                    .as_deref()
                    .is_some_and(|t| t.contains("await_specialists"))
        })
        .expect("the hand-over must be persisted as a call/result pair");
    assert_eq!(
        rows[collecting].content.as_deref().unwrap_or(""),
        "",
        "a mid-turn hand-over carries no answer text — the model has not written one yet"
    );
    assert_eq!(rows[collecting + 1].role, "tool");
    let last = rows.last().unwrap();
    assert_eq!(
        (last.role.as_str(), last.content.as_deref()),
        ("assistant", Some("done")),
        "the answer is written once, with the report already in context"
    );
}

/// Every tool card must be closable: `tool_start` and `tool_finish` carry the
/// model's own call id, because a step's tool calls run concurrently and
/// arrival order cannot pair them. Without it the longest call in a batch — an
/// `await_specialists` next to the `call_specialist`s that opened it — stayed
/// pending in the UI forever.
#[tokio::test]
async fn tool_frames_carry_the_call_id_that_pairs_them() {
    let pool = test_pool().await;
    let mut server = mockito::Server::new_async().await;
    mock_tool_then_stop(&mut server, "some_unregistered_tool").await;
    let (agent, _orchestrator) = ticket_agent(&pool, &server.url()).await;
    seed_session(
        &pool,
        "parent",
        json!({"provider": "mock", "hitl_policy": "auto_reject"}),
    )
    .await;

    let events = run_and_collect(&agent, "parent").await;
    for kind in [SSEEventType::ToolStart, SSEEventType::ToolFinish] {
        let frames: Vec<&SSEEvent> = events
            .iter()
            .filter(|e| e.event_type == kind && e.tool_name.as_deref() != Some("__budget__"))
            .collect();
        assert!(!frames.is_empty(), "expected at least one {kind:?} frame");
        for f in frames {
            assert!(
                f.tool_call_id.as_deref().is_some_and(|id| !id.is_empty()),
                "{kind:?} for {:?} carried no tool_call_id",
                f.tool_name
            );
        }
    }
}

/// A timed-out delegate is asked what it managed to write, not simply discarded.
///
/// The shape of the bug this pins: a specialist's *last* act is
/// `finalize_structured`, a schema-constrained pass that re-sends the whole
/// history and retries once. It is the largest request of the run, issued at
/// the moment the budget is most nearly spent — and `run_turn_and_wait` spawns
/// the loop and awaits a watcher, so the orchestrator's timeout drops the
/// watcher while the child is still going. A delegate that had already written
/// a valid report therefore reported only "ran longer than Ns and was stopped",
/// with the finished report sitting in the child's transcript.
///
/// Here the first request answers in the promised shape and the second — the
/// structured pass — stalls until the budget expires.
#[tokio::test]
async fn a_report_written_just_before_the_deadline_is_not_thrown_away() {
    let pool = test_pool().await;
    let mut server = mockito::Server::new_async().await;
    let answer = json!({"findings": "three sources agree"}).to_string();
    mock_answer(&mut server, &answer.replace('"', "\\\"")).await;
    let _stalled = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_chunked_body(|_w| {
            std::thread::sleep(std::time::Duration::from_secs(30));
            Ok(())
        })
        .create_async()
        .await;

    let outcome = run_until_timeout(
        &pool,
        &server,
        Some(json!({
            "type": "object",
            "properties": {"findings": {"type": "string"}},
            "required": ["findings"],
            "additionalProperties": false,
        })),
    )
    .await;

    match outcome {
        Ok(Ok(outcome)) => {
            assert_eq!(outcome.answer.trim(), answer);
            assert!(
                outcome
                    .notes
                    .iter()
                    .any(|n| n.contains("just after") && n.contains("budget")),
                "the caller is owed the fact that it finished late: {:?}",
                outcome.notes
            );
        }
        other => panic!("a finished report must survive its own timeout, got {other:?}"),
    }
}

/// The other half of the contract: prose where a shape was promised is still a
/// failure, because `specialists::server` embeds a *parsed* report and handing
/// back unvalidated text would put the wrong kind of thing there. It is carried
/// on the failure instead, where it reads as what it is.
#[tokio::test]
async fn prose_left_by_a_timed_out_delegate_is_reported_as_how_far_it_got() {
    let pool = test_pool().await;
    let mut server = mockito::Server::new_async().await;
    mock_answer(&mut server, "I read two of the five sources").await;
    let _stalled = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_chunked_body(|_w| {
            std::thread::sleep(std::time::Duration::from_secs(30));
            Ok(())
        })
        .create_async()
        .await;

    let outcome = run_until_timeout(
        &pool,
        &server,
        Some(json!({
            "type": "object",
            "properties": {"findings": {"type": "string"}},
            "required": ["findings"],
            "additionalProperties": false,
        })),
    )
    .await;

    match outcome {
        Ok(Err(why)) => {
            assert!(
                why.contains("longer than") && why.contains("stopped"),
                "still a failure, and still says why: {why}"
            );
            assert!(
                why.contains("How far it got") && why.contains("two of the five"),
                "the partial work must be carried rather than dropped: {why}"
            );
        }
        other => panic!("expected a failure carrying the partial, got {other:?}"),
    }
}

/// Run one delegate against `server` with a one-second budget, so the timeout
/// path is reached quickly and deterministically.
async fn run_until_timeout(
    pool: &SqlitePool,
    server: &mockito::ServerGuard,
    response_schema: Option<serde_json::Value>,
) -> Result<Result<bigtiny2::agent::orchestrator::DelegateOutcome, String>, SpawnRefusal> {
    let agent = build_agent(pool, Some(&server.url()));
    let router = Arc::new(ProviderRouter::new(BigTinyConfig::default().cache));
    router.register_openai(
        "mock",
        bigtiny2::config::ProviderConfig {
            base_url: server.url(),
            ..Default::default()
        },
    );
    let orchestrator = Arc::new(Orchestrator::new(pool.clone(), 3, 0.25, vec![], 1));
    orchestrator.attach(&agent);
    orchestrator.attach_router(router);

    sessions::create_session_for_app(pool, "parent", "Parent", APP)
        .await
        .unwrap();

    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        orchestrator.run(DelegateRun {
            name: "researcher".into(),
            parent_session_id: "parent".into(),
            prompt: "go".into(),
            system_prompt: None,
            provider: None,
            model: None,
            tool_allow: vec![],
            response_schema,
            max_steps: 5,
            reasoning_cap: None,
        }),
    )
    .await
    .expect("the orchestrator itself must not hang")
}

/// The deadline valve, end to end through a real turn.
///
/// The unit tests pin `decide_turn_mode`'s precedence and the reserve
/// arithmetic; this pins the thing between them -- that `deadline_unix_ms`
/// survives the metadata round trip, that the loop reads it, and that the valve
/// fires with the deadline's own wording rather than the context valve's, which
/// would tell a delegate to "send another message to continue" as it is about
/// to be killed.
///
/// Timing: the turn is given 1.2s, so the reserve is 400ms. Step 0's provider
/// call is made to take a second, which leaves step 1 inside the reserve by a
/// margin much larger than the scheduling noise either side of it.
#[tokio::test]
async fn a_turn_near_its_deadline_withdraws_tools_and_says_why() {
    let pool = test_pool().await;
    let mut server = mockito::Server::new_async().await;

    // Step 0: a tool call, arriving slowly enough to eat most of the budget.
    server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_chunked_body(|w| {
            std::thread::sleep(std::time::Duration::from_secs(1));
            w.write_all(
                b"data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"some_unregistered_tool\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n\
                  data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                  data: [DONE]\n\n",
            )
        })
        .expect(1)
        .create_async()
        .await;
    // Step 1: the wrap-up reply the valve asks for.
    mock_answer(&mut server, "what I have so far").await;

    let agent = build_agent(&pool, Some(&server.url()));
    let deadline_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 1200;
    seed_session(
        &pool,
        "near-deadline",
        json!({
            "provider": "mock",
            "hitl_policy": "auto_reject",
            "deadline_unix_ms": deadline_ms,
        }),
    )
    .await;

    let events = run_and_collect(&agent, "near-deadline").await;
    let notice = events
        .iter()
        .filter(|e| e.event_type == SSEEventType::ToolFinish)
        .find_map(|e| {
            (e.tool_name.as_deref() == Some("__context_budget__"))
                .then(|| e.tool_result.clone().unwrap_or_default())
        })
        .expect("the valve must tell the user why the turn ended early");
    assert!(
        notice.contains("time budget"),
        "the deadline's wording, not the context valve's: {notice}"
    );
    assert!(
        notice.contains("specialist_timeout_secs"),
        "and it names the setting that would stop it recurring: {notice}"
    );
}

/// The companion: a turn with no deadline in its metadata -- every ordinary
/// chat turn -- must never see the valve. The budget bound is for delegates,
/// which run unattended; a user can stop their own turn, and cutting them off
/// mid-answer would be a worse failure than a slow one.
#[tokio::test]
async fn an_ordinary_turn_never_sees_the_deadline_valve() {
    let pool = test_pool().await;
    let mut server = mockito::Server::new_async().await;
    mock_tool_then_stop(&mut server, "some_unregistered_tool").await;
    let agent = build_agent(&pool, Some(&server.url()));
    seed_session(
        &pool,
        "no-deadline",
        json!({"provider": "mock", "hitl_policy": "auto_reject"}),
    )
    .await;

    let events = run_and_collect(&agent, "no-deadline").await;
    assert!(
        !events
            .iter()
            .any(|e| e.tool_name.as_deref() == Some("__context_budget__")),
        "a turn with no deadline has no budget to run out of"
    );
}
