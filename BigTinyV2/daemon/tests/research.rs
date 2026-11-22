//! Deep research end to end: a real turn 0 that submits a plan and pauses for
//! approval, then the driver's wave, critique and write-up, against a mock
//! provider that answers each kind of request by what its prompt says.
//!
//! What is pinned here is what fails quietly if it regresses: the plan must
//! wait for the user, the frontier must carry a tangent, a fabricated
//! citation must not reach the deliverable, and the run must end with a
//! report in the chat and on disk.

use std::sync::Arc;

use bigtiny2::agent::orchestrator::Orchestrator;
use bigtiny2::agent::summarizer_chain::SummarizerChain;
use bigtiny2::agent::Agent;
use bigtiny2::config::BigTinyConfig;
use bigtiny2::hitl::manager::HITLManager;
use bigtiny2::mcp::MCPManager;
use bigtiny2::provider::router::ProviderRouter;
use bigtiny2::research::driver::Driver;
use bigtiny2::research::ledger::Body;
use bigtiny2::research::ops;
use bigtiny2::server::events::{SSEEvent, SSEEventType};
use bigtiny2::storage::research as store;
use bigtiny2::storage::{messages, sessions};
use mockito::Matcher;
use serde_json::{json, Value};
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

fn build_agent(pool: &SqlitePool, base_url: &str, orchestrator: Arc<Orchestrator>) -> Arc<Agent> {
    let config = BigTinyConfig::default();
    let router = Arc::new(ProviderRouter::new(config.cache.clone()));
    router.register_openai(
        "mock",
        bigtiny2::config::ProviderConfig {
            base_url: base_url.to_string(),
            ..Default::default()
        },
    );
    let mcp = Arc::new(MCPManager::new(pool.clone(), None));
    mcp.attach_orchestrator(orchestrator);
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

fn sse_content(text: &str) -> String {
    let delta = json!({"choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]});
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn sse_tool_call(name: &str, args: &Value) -> String {
    let delta = json!({"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[
        {"index":0,"id":"c1","type":"function","function":{"name":name,"arguments":args.to_string()}}
    ]},"finish_reason":null}]});
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

/// A mock for requests whose body matches every pattern.
async fn on(server: &mut mockito::ServerGuard, patterns: &[&str], body: String) {
    let matcher = Matcher::AllOf(
        patterns
            .iter()
            .map(|p| Matcher::Regex((*p).to_string()))
            .collect(),
    );
    server
        .mock("POST", "/v1/chat/completions")
        .match_body(matcher)
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .create_async()
        .await;
}

fn plan() -> Value {
    json!({"plan": {
        "title": "Thinking machines in early SF",
        "objective": "An annotated bibliography of 1940-1970 SF about AI",
        "deliverable": {"description": "One entry per work", "format": "markdown", "allow_unverified": false},
        "sections": [{"id": "works", "title": "Works", "description": "The bibliography"}],
        "coverage": [{"id": "magazines", "label": "Magazine fiction"}],
        "questions": ["Which 1940s stories feature robots?"],
        "acceptance": ["Every entry cites a source"],
        "boundaries": ["No film"],
        "turns": 3
    }})
}

fn frontier() -> Value {
    let task = |objective: &str, tangent: bool| {
        json!({"kind": "search", "objective": objective, "output_format": "findings",
               "guidance": "bibliographies", "boundaries": "1940-1970", "refs": [],
               "tangent": tangent, "pursues": []})
    };
    json!({"tasks": [
        task("Robot stories in Astounding", false),
        task("Robot stories in other magazines", false),
        task("Radio drama about thinking machines", true),
    ]})
}

fn report() -> Value {
    json!({
        "findings": [{
            "claim": "Asimov's Robbie first appeared in Super Science Stories in 1940",
            "evidence": "Strange Playfellow, Super Science Stories, September 1940",
            "source_url": "https://www.isfdb.org/cgi-bin/title.cgi?1",
            "source_title": "ISFDB: Robbie",
            "attribution": "",
            "cells": ["magazines"],
            "sections": ["works"],
            "answers_objective": true
        }],
        "open_questions": [],
        "dead_ends": [],
        "notes": "",
        "refusals": []
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_goes_from_brief_to_a_verified_report() {
    let pool = test_pool().await;
    let mut server = mockito::Server::new_async().await;

    // Turn 0: submit the plan, then (after the tool result) a sentence.
    on(
        &mut server,
        &["deep research request", r#""role":"tool""#],
        sse_content("Started."),
    )
    .await;
    on(
        &mut server,
        &["deep research request"],
        sse_tool_call("submit_research_plan", &plan()),
    )
    .await;
    // The wave's planner: set the frontier, then stop.
    on(
        &mut server,
        &["You are planning wave", r#""role":"tool""#],
        sse_content("Wave set."),
    )
    .await;
    on(
        &mut server,
        &["You are planning wave"],
        sse_tool_call("research_set_frontier", &frontier()),
    )
    .await;
    // Delegates, by their persona.
    on(
        &mut server,
        &["You research a question"],
        sse_content(&report().to_string()),
    )
    .await;
    on(
        &mut server,
        &["adversarial reviewer"],
        sse_content(
            &json!({"defects": [], "kept_tangents": [], "notes": "", "refusals": []}).to_string(),
        ),
    )
    .await;
    // The writer cites the real finding (L5: three tasks, a source, then the
    // finding) and one that does not exist.
    on(
        &mut server,
        &["You write one section"],
        sse_content(
            &json!({"markdown": "Robbie appeared in 1940 [L5]. It was reprinted [L99].",
                    "notes": "", "refusals": []})
            .to_string(),
        ),
    )
    .await;
    on(
        &mut server,
        &["You check a section"],
        sse_content(&json!({"verdicts": [], "notes": "", "refusals": []}).to_string()),
    )
    .await;
    on(
        &mut server,
        &["You write the opening"],
        sse_content(
            &json!({"markdown": "What was asked, and found.", "notes": "", "refusals": []})
                .to_string(),
        ),
    )
    .await;

    let orchestrator = Arc::new(Orchestrator::new(pool.clone(), 3, 0.25, vec![], 300));
    let agent = build_agent(&pool, &server.url(), orchestrator.clone());
    orchestrator.attach(&agent);
    sqlx::query(
        "INSERT INTO mcp_servers (id, name, transport, command, enabled, status) \
         VALUES ('research', 'research', 'in_process', 'research', 1, 'disconnected')",
    )
    .execute(&pool)
    .await
    .unwrap();
    agent.mcp().connect_server("research").await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    sessions::create_session_for_app(&pool, "chat", "chat", APP)
        .await
        .unwrap();
    sessions::update_session_config(
        &pool,
        "chat",
        &json!({
            "provider": "mock",
            "chat_dir": dir.path().to_string_lossy(),
            ops::META_RUN: "run1",
            ops::META_ROLE: ops::ROLE_PLAN,
        })
        .to_string(),
    )
    .await
    .unwrap();
    store::create_run(&pool, "run1", APP, "chat", "AI in SF, 1940-1970", 3)
        .await
        .unwrap();

    // Turn 0 pauses on the plan; nothing happens until the user approves.
    let (tx, mut rx) = mpsc::unbounded_channel::<SSEEvent>();
    let instructions = bigtiny2::research::prompts::plan_instructions(3, true);
    agent
        .run_turn(
            "chat".into(),
            format!("{instructions}\n\nAI in SF"),
            None,
            None,
            tx,
            None,
        )
        .expect("turn 0 starts");
    let mut action = None;
    while let Some(ev) = rx.recv().await {
        if ev.event_type == SSEEventType::HitlPause {
            assert_eq!(ev.tool_name.as_deref(), Some("submit_research_plan"));
            action = ev.action_id;
            break;
        }
    }
    let action = action.expect("the plan waits for the user");
    assert_eq!(
        store::get_run(&pool, "run1").await.unwrap().unwrap().status,
        store::STATUS_PLANNING
    );
    agent
        .hitl()
        .lock()
        .await
        .record_decision(&action, "allow", None);
    agent.resolve_approval(&action);
    while rx.recv().await.is_some() {}
    let run = store::get_run(&pool, "run1").await.unwrap().unwrap();
    assert_eq!(
        run.status,
        store::STATUS_RESEARCHING,
        "approval starts the research"
    );
    assert!(run.contract.is_some());

    let driver = Arc::new(Driver {
        pool: pool.clone(),
        agent: agent.clone(),
        orchestrator,
    });
    driver.drive_now("run1").await.unwrap();

    let run = store::get_run(&pool, "run1").await.unwrap().unwrap();
    assert_eq!(run.status, store::STATUS_DONE, "error: {:?}", run.error);
    assert_eq!(run.turns_used, 3);
    let steps = store::steps(&pool, "run1").await.unwrap();
    let phases: Vec<&str> = steps.iter().map(|s| s.phase.as_str()).collect();
    assert_eq!(phases, ["wave", "critic", "write"]);

    let ledger = ops::load_ledger(&pool, "run1").await.unwrap();
    assert!(
        ledger
            .of_kind("task")
            .any(|e| matches!(&e.body, Body::Task(t) if t.tangent)),
        "the frontier carried a tangent"
    );
    assert_eq!(
        ledger.of_kind("finding").count(),
        1,
        "three reports of one claim are one finding"
    );
    assert_eq!(ledger.of_kind("source").count(), 1);

    // The report: in the chat and on disk, with numbered sources and no trace
    // of the fabricated citation.
    let path = run.output_path.expect("the report was saved");
    let doc = std::fs::read_to_string(&path).unwrap();
    assert!(doc.contains("# Thinking machines in early SF"), "{doc}");
    assert!(doc.contains("Robbie appeared in 1940 [1]"), "{doc}");
    assert!(!doc.contains("L99") && !doc.contains("[L"), "{doc}");
    assert!(
        doc.contains("## Sources") && doc.contains("isfdb.org"),
        "{doc}"
    );
    let history = messages::get_messages_by_session(&pool, "chat")
        .await
        .unwrap();
    let last = history.last().unwrap();
    assert_eq!(last.role, "assistant");
    assert!(last
        .content
        .as_deref()
        .unwrap_or_default()
        .contains("## Sources"));
    // Milestone notes, in order, before the report.
    let texts: Vec<String> = history
        .iter()
        .filter(|m| m.role == "assistant")
        .filter_map(|m| m.content.clone())
        .collect();
    let wave = texts
        .iter()
        .position(|t| t.starts_with("**Deep research: wave 1 of 1 done.**"))
        .expect("a note after the wave");
    let critique = texts
        .iter()
        .position(|t| t.starts_with("**Deep research: critique done.**"))
        .expect("a note after the critique");
    assert!(wave < critique && critique < texts.len() - 1, "{texts:?}");
    assert!(
        texts[wave].contains("3 tasks (1 tangent), 1 new finding from 1 source"),
        "{}",
        texts[wave]
    );

    // Turn 0's role is gone from the chat.
    let meta: Value = serde_json::from_str(
        &sessions::get_session_metadata(&pool, "chat")
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert!(meta.get(ops::META_ROLE).is_none());
}

#[tokio::test]
async fn a_new_run_defaults_to_four_turns() {
    assert_eq!(bigtiny2::research::contract::clamp_turns(None), 4);
    assert_eq!(bigtiny2::research::contract::waves_for(4), 2);
}
