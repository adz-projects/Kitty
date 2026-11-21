//! A debate: a turn that lets its caller choose each delegate's host.
//!
//! The rules pinned here are the ones whose failure would be silent or costly:
//! a caller-hosted specialist running outside an armed turn (so premium hosts
//! become reachable from any chat), a host outside the panel being accepted, a
//! panel outliving its turn, and a chosen host being quietly swapped for the
//! daemon's usual cheap pick.

use std::sync::Arc;

use bigtiny2::agent::orchestrator::{DelegateRun, Orchestrator, SpawnRefusal};
use bigtiny2::agent::summarizer_chain::SummarizerChain;
use bigtiny2::agent::Agent;
use bigtiny2::config::BigTinyConfig;
use bigtiny2::hitl::manager::HITLManager;
use bigtiny2::mcp::MCPManager;
use bigtiny2::models::specialist::Specialist;
use bigtiny2::provider::router::ProviderRouter;
use bigtiny2::specialists::server::{
    debate_orders, CallRequest, DebateOrderRequest, ListRequest, SpecialistServer,
};
use bigtiny2::storage::{sessions, specialists as store};
use rmcp::handler::server::wrapper::Parameters;
use serde_json::Value;
use sqlx::SqlitePool;

const APP: &str = "test-app";

async fn test_pool() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    bigtiny2::storage::apps::register_app(&pool, APP, "Test App", "k")
        .await
        .unwrap();
    sessions::create_session_for_app(&pool, "parent", "Parent", APP)
        .await
        .unwrap();
    pool
}

fn build_agent(pool: &SqlitePool, router: Arc<ProviderRouter>) -> Arc<Agent> {
    let config = BigTinyConfig::default();
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

/// Two hosts: an ordinary cheap one, and a "seat" the user marked never to
/// host specialists, whose model is on the denylist — everything a normal
/// delegate is kept away from.
fn router() -> Arc<ProviderRouter> {
    let router = Arc::new(ProviderRouter::new(BigTinyConfig::default().cache));
    router.register_openai(
        "cheap",
        bigtiny2::config::ProviderConfig {
            base_url: "http://127.0.0.1:9".into(),
            model: "small-model".into(),
            ..Default::default()
        },
    );
    router.register_openai(
        "seat",
        bigtiny2::config::ProviderConfig {
            base_url: "http://127.0.0.1:9".into(),
            model: "premium-model".into(),
            subagent_role: Some("never".into()),
            ..Default::default()
        },
    );
    router
}

/// The agent is returned only to keep it alive: the orchestrator holds it
/// weakly, and a dropped agent reads as shutdown.
async fn setup() -> (SqlitePool, Arc<Orchestrator>, SpecialistServer, Arc<Agent>) {
    let pool = test_pool().await;
    let router = router();
    let agent = build_agent(&pool, router.clone());
    let orchestrator = Arc::new(Orchestrator::new(
        pool.clone(),
        3,
        0.25,
        vec!["premium-*".to_string()],
        300,
    ));
    orchestrator.attach(&agent);
    orchestrator.attach_router(router);
    store::upsert(
        &pool,
        &Specialist {
            id: "deb".into(),
            app_id: Some(APP.into()),
            name: "debater".into(),
            description: "A debate participant.".into(),
            system_prompt: "Stay in persona.".into(),
            provider: None,
            model: None,
            tool_allow: vec![],
            response_schema: None,
            max_steps: 3,
            reasoning_cap: None,
            fan_out: None,
            max_concurrent: None,
            host_policy: Some("caller".into()),
            enabled: true,
            builtin: false,
        },
    )
    .await
    .unwrap();
    let server = SpecialistServer::new(pool.clone(), orchestrator.clone());
    (pool, orchestrator, server, agent)
}

fn call(host: Option<&str>) -> Parameters<CallRequest> {
    Parameters(CallRequest {
        specialist: "debater".into(),
        request: "Argue for tenure.".into(),
        refs: None,
        host: host.map(str::to_string),
        session_id: Some("parent".into()),
    })
}

fn parsed(s: &str) -> Value {
    serde_json::from_str(s).expect("tool results are JSON")
}

#[tokio::test]
async fn a_caller_hosted_specialist_is_refused_outside_a_debate() {
    let (_pool, _orch, server, _agent) = setup().await;
    let out = parsed(&server.call_specialist(call(Some("seat"))).await);
    assert_eq!(out["ok"], false);
    let err = out["error"].as_str().unwrap();
    assert!(err.contains("/debate"), "say how to get one: {err}");
}

#[tokio::test]
async fn a_host_must_be_named_and_must_be_on_the_panel() {
    let (_pool, orch, server, _agent) = setup().await;
    let _guard = orch.arm_debate("parent", vec!["seat".into()]);

    let out = parsed(&server.call_specialist(call(None)).await);
    assert_eq!(out["ok"], false);
    assert_eq!(
        out["panel"][0], "seat",
        "a missing host is answered with the panel"
    );

    let out = parsed(&server.call_specialist(call(Some("cheap"))).await);
    assert_eq!(out["ok"], false, "a host off the panel is refused: {out}");

    let out = parsed(&server.call_specialist(call(Some("seat"))).await);
    assert_eq!(out["ok"], true, "{out}");
}

#[tokio::test]
async fn a_panel_ends_with_its_turn() {
    let (_pool, orch, server, _agent) = setup().await;
    {
        let _guard = orch.arm_debate("parent", vec!["seat".into()]);
        assert!(orch.debate_panel("parent").is_some());
    }
    assert!(orch.debate_panel("parent").is_none());
    let out = parsed(&server.call_specialist(call(Some("seat"))).await);
    assert_eq!(out["ok"], false, "a later turn may not use the old panel");
}

/// The seat is marked "never" and its model is denylisted, and it still runs
/// there — and nowhere else, not even the eligible cheap host.
#[tokio::test]
async fn a_chosen_host_skips_role_and_denylist_and_is_not_swapped() {
    let (pool, orch, _server, _agent) = setup().await;
    let run = |force_host: bool| DelegateRun {
        name: "debater".into(),
        parent_session_id: "parent".into(),
        prompt: "go".into(),
        system_prompt: None,
        provider: Some("seat".into()),
        model: None,
        tool_allow: vec![],
        response_schema: None,
        max_steps: 2,
        reasoning_cap: None,
        max_concurrent: None,
        force_host,
    };

    // The ordinary path drops the pin and lands on the cheap host.
    let _ = orch.run(run(false)).await;
    // The forced path runs on the seat. Nothing answers at either address, so
    // the turn itself fails; the host it was given is what it used.
    if let Err(SpawnRefusal::NoHost(why)) = orch.run(run(true)).await {
        panic!("a chosen host was refused: {why}");
    }
    let hosts: Vec<(Option<String>,)> = sqlx::query_as(
        "SELECT provider_id FROM execution_history WHERE trigger_type = 'subagent' \
         ORDER BY started_at, rowid",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let hosts: Vec<_> = hosts
        .into_iter()
        .map(|(p,)| p.unwrap_or_default())
        .collect();
    assert_eq!(hosts, ["cheap", "seat"]);

    // And the forced child carries no denylist for the loop to refuse it by.
    let kids = sessions::children_of(&pool, "parent", APP).await.unwrap();
    let mut saw_pinned = false;
    for kid in kids {
        let meta: Value = sessions::get_session(&pool, &kid)
            .await
            .unwrap()
            .and_then(|r| r.metadata)
            .and_then(|m| serde_json::from_str(&m).ok())
            .unwrap();
        if meta["provider"] == "seat" {
            saw_pinned = true;
            assert_eq!(meta["host_pinned"], true);
            assert!(meta.get("subagent_model_deny").is_none());
        }
    }
    assert!(saw_pinned);
}

#[tokio::test]
async fn the_debater_is_listed_only_during_a_debate() {
    let (_pool, orch, server, _agent) = setup().await;
    let names = |s: String| -> Vec<String> {
        parsed(&s)["specialists"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect()
    };
    let list = || {
        Parameters(ListRequest {
            session_id: Some("parent".into()),
        })
    };
    assert!(!names(server.list_specialists(list()).await).contains(&"debater".to_string()));
    let _guard = orch.arm_debate("parent", vec!["seat".into()]);
    assert!(names(server.list_specialists(list()).await).contains(&"debater".to_string()));
}

#[tokio::test]
async fn debate_order_deals_a_fresh_permutation_per_later_round() {
    let (_pool, orch, server, _agent) = setup().await;
    let req = || {
        Parameters(DebateOrderRequest {
            personas: vec!["A".into(), "B".into(), "C".into()],
            rounds: 4,
            session_id: Some("parent".into()),
        })
    };
    assert_eq!(parsed(&server.debate_order(req()).await)["ok"], false);

    let _guard = orch.arm_debate("parent", vec!["seat".into()]);
    let out = parsed(&server.debate_order(req()).await);
    let orders = out["orders"].as_array().unwrap();
    assert_eq!(orders.len(), 3, "rounds 2, 3 and 4");
    for (i, o) in orders.iter().enumerate() {
        assert_eq!(o["round"], i + 2);
        let mut names: Vec<&str> = o["order"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["A", "B", "C"]);
    }

    // Shuffled, not fixed: across many deals, more than one order appears.
    let personas: Vec<String> = ["A", "B", "C"].iter().map(|s| s.to_string()).collect();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..50 {
        seen.extend(debate_orders(&personas, 2));
    }
    assert!(seen.len() > 1);
}
