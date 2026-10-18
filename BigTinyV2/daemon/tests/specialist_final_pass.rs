//! A schema'd delegate end to end: the tool loop, then the schema-constrained
//! final pass, through a real `Orchestrator::run` against a mock provider that
//! records every request it is sent.
//!
//! The final pass is where delegates used to fail. It ran with no effort level
//! (the model's own default — xhigh on a self-hosted Qwen3.8), had 1024-4096
//! tokens of room that reasoning ate before the JSON started, gave up on the
//! first transport error, sent current Claude models a forced `tool_choice`
//! they reject, and discarded the delegate's whole investigation when it
//! failed. Each test below pins one of those down on the wire.

use std::sync::{Arc, Mutex};

use bigtiny2::agent::orchestrator::{DelegateOutcome, DelegateRun, Orchestrator, SpawnRefusal};
use bigtiny2::agent::summarizer_chain::SummarizerChain;
use bigtiny2::agent::Agent;
use bigtiny2::config::{BigTinyConfig, ProviderConfig};
use bigtiny2::hitl::manager::HITLManager;
use bigtiny2::mcp::MCPManager;
use bigtiny2::provider::router::ProviderRouter;
use bigtiny2::storage::sessions;
use serde_json::{json, Value};
use sqlx::SqlitePool;

const APP: &str = "test-app";
/// What the delegate's tool loop says before the final pass.
const PROSE: &str = "I checked both sources; the answer is 42.";

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

/// An agent and an orchestrator sharing one router, the way `lib.rs` wires the
/// daemon — the orchestrator reads the host's `delegate_effort` from it.
fn orchestrator_on(pool: &SqlitePool, router: Arc<ProviderRouter>) -> Arc<Orchestrator> {
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
    let agent = Arc::new(Agent::new(
        pool.clone(),
        router.clone(),
        mcp,
        hitl,
        summarizer,
        config,
        std::env::temp_dir().to_string_lossy().into_owned(),
        bigtiny2::plugins::test_plugin_host(pool),
        bigtiny2::plugins::test_memorabilia_host(pool),
    ));
    let orchestrator = Arc::new(Orchestrator::new(pool.clone(), 3, 0.25, vec![], 60));
    orchestrator.attach(&agent);
    orchestrator.attach_router(router);
    // Leaked on purpose: the orchestrator holds the agent weakly, exactly as
    // the daemon does, so something has to own it for the test's duration.
    std::mem::forget(agent);
    orchestrator
}

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {"findings": {"type": "string", "maxLength": 200}},
        "required": ["findings"],
        "additionalProperties": false,
    })
}

async fn run_delegate(orchestrator: &Orchestrator) -> Result<DelegateOutcome, String> {
    match orchestrator
        .run(DelegateRun {
            name: "researcher".into(),
            parent_session_id: "parent".into(),
            prompt: "what is the answer?".into(),
            system_prompt: None,
            provider: None,
            model: None,
            tool_allow: vec![],
            response_schema: Some(schema()),
            max_steps: 5,
            reasoning_cap: None,
            max_concurrent: None,
        })
        .await
    {
        Ok(result) => result,
        Err(SpawnRefusal::NoHost(why)) => panic!("no host: {why}"),
        Err(other) => panic!("refused: {other}"),
    }
}

/// Every request body the mock received, in order.
type Bodies = Arc<Mutex<Vec<Value>>>;

/// One mock answering every request on `path` from `respond(body, n)`, where
/// `n` counts the final-pass requests seen so far (0 for the first).
async fn mock_provider(
    server: &mut mockito::ServerGuard,
    path: &str,
    is_final: fn(&Value) -> bool,
    respond: impl Fn(&Value, usize) -> String + Send + Sync + 'static,
) -> Bodies {
    let bodies: Bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = bodies.clone();
    server
        .mock("POST", path)
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body_from_request(move |req| {
            let body: Value = req
                .utf8_lossy_body()
                .ok()
                .and_then(|b| serde_json::from_str(&b).ok())
                .unwrap_or(Value::Null);
            let mut all = seen.lock().unwrap();
            let finals = all.iter().filter(|b| is_final(b)).count();
            let out = respond(&body, finals);
            all.push(body);
            out.into_bytes()
        })
        .expect_at_least(1)
        .create_async()
        .await;
    bodies
}

fn openai_sse(text: &str, finish: &str) -> String {
    let content = json!({"choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": null}]});
    let stop = json!({"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]});
    format!("data: {content}\n\ndata: {stop}\n\ndata: [DONE]\n\n")
}

fn openai_final(b: &Value) -> bool {
    b.get("response_format").is_some()
}

fn anthropic_sse(text: &str, stop: &str) -> String {
    let start = json!({"type": "message_start", "message": {"usage": {"input_tokens": 10, "output_tokens": 1}}});
    let delta = json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}});
    let end = json!({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 5}});
    format!(
        "event: message_start\ndata: {start}\n\n\
         event: content_block_delta\ndata: {delta}\n\n\
         event: message_delta\ndata: {end}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    )
}

fn anthropic_final(b: &Value) -> bool {
    b["output_config"].get("format").is_some()
}

/// A hosted OpenAI host whose card says delegates run at `low`.
fn openai_router(url: &str) -> Arc<ProviderRouter> {
    let router = Arc::new(ProviderRouter::new(BigTinyConfig::default().cache));
    router.register_openai(
        "mock",
        ProviderConfig {
            provider_type: "openai".into(),
            base_url: url.to_string(),
            model: "gpt-test".into(),
            delegate_effort: Some("low".into()),
            ..Default::default()
        },
    );
    router
}

fn answer_of(outcome: &DelegateOutcome) -> Value {
    serde_json::from_str(&outcome.answer).expect("the report is JSON")
}

/// The happy path, pinned on the wire: every request of the run — the tool
/// loop and the final pass — asks for the host's delegate level rather than
/// the model's default, and the final pass gets a real 8k of room instead of
/// the old 1024-4096 reserve. The child is marked as a delegate.
#[tokio::test]
async fn a_schemad_delegate_runs_low_and_its_final_pass_gets_real_room() {
    let pool = test_pool().await;
    let mut server = mockito::Server::new_async().await;
    let bodies = mock_provider(
        &mut server,
        "/v1/chat/completions",
        openai_final,
        |body, _| {
            if openai_final(body) {
                openai_sse(&json!({"findings": "42"}).to_string(), "stop")
            } else {
                openai_sse(PROSE, "stop")
            }
        },
    )
    .await;
    let orchestrator = orchestrator_on(&pool, openai_router(&server.url()));

    let outcome = run_delegate(&orchestrator)
        .await
        .expect("the delegate reports");
    assert_eq!(answer_of(&outcome)["findings"], "42");

    let bodies = bodies.lock().unwrap().clone();
    assert_eq!(
        bodies.len(),
        2,
        "one tool-loop step, one final pass: {bodies:?}"
    );
    for b in &bodies {
        assert_eq!(b["reasoning_effort"], "low", "every request runs low: {b}");
    }
    let final_pass = bodies.iter().find(|b| openai_final(b)).unwrap();
    assert_eq!(final_pass["max_tokens"], 8192, "{final_pass}");
    assert!(
        final_pass.get("tools").is_none(),
        "the final pass carries no tools"
    );

    let child = &sessions::children_of(&pool, "parent", APP).await.unwrap()[0];
    let meta: Value = serde_json::from_str(
        &sessions::get_session(&pool, child)
            .await
            .unwrap()
            .unwrap()
            .metadata
            .unwrap(),
    )
    .unwrap();
    assert_eq!(meta["thinking_effort"], "low");
    assert_eq!(meta["delegate"], true);
}

/// Reasoning ate the room and the JSON stopped mid-object. That is retried as
/// the same request with twice the room — not as a "you returned no JSON"
/// correction the model cannot act on — and the run succeeds.
#[tokio::test]
async fn a_truncated_final_answer_is_retried_with_more_room() {
    let pool = test_pool().await;
    let mut server = mockito::Server::new_async().await;
    let bodies = mock_provider(
        &mut server,
        "/v1/chat/completions",
        openai_final,
        |body, finals| match (openai_final(body), finals) {
            (false, _) => openai_sse(PROSE, "stop"),
            (true, 0) => openai_sse("{\"findings\": \"4", "length"),
            (true, _) => openai_sse(&json!({"findings": "42"}).to_string(), "stop"),
        },
    )
    .await;
    let orchestrator = orchestrator_on(&pool, openai_router(&server.url()));

    let outcome = run_delegate(&orchestrator)
        .await
        .expect("the retry reports");
    assert_eq!(answer_of(&outcome)["findings"], "42");

    let bodies = bodies.lock().unwrap().clone();
    let finals: Vec<&Value> = bodies.iter().filter(|b| openai_final(b)).collect();
    assert_eq!(finals.len(), 2);
    assert_eq!(finals[0]["max_tokens"], 8192);
    assert_eq!(
        finals[1]["max_tokens"], 16384,
        "the retry gets twice the room"
    );
    // The same request, not one with a correction appended.
    assert_eq!(finals[0]["messages"], finals[1]["messages"]);
}

/// Both passes fail. The run is still a failure — prose is not the shape the
/// caller contracted for — but the delegate's investigation comes back with it
/// as how far it got, instead of being discarded with the error.
#[tokio::test]
async fn a_failed_final_pass_still_returns_how_far_the_delegate_got() {
    let pool = test_pool().await;
    let mut server = mockito::Server::new_async().await;
    let _bodies = mock_provider(
        &mut server,
        "/v1/chat/completions",
        openai_final,
        |body, _| {
            if openai_final(body) {
                openai_sse("Sorry, I cannot format that.", "stop")
            } else {
                openai_sse(PROSE, "stop")
            }
        },
    )
    .await;
    let orchestrator = orchestrator_on(&pool, openai_router(&server.url()));

    let err = run_delegate(&orchestrator)
        .await
        .expect_err("prose is not a report");
    assert!(err.contains("How far it got"), "{err}");
    assert!(err.contains("the answer is 42"), "{err}");
}

/// A transport failure on the final pass used to end it with no second try.
/// It is retried once, and the run succeeds.
#[tokio::test]
async fn a_transport_error_on_the_final_pass_is_retried_once() {
    let pool = test_pool().await;
    let mut server = mockito::Server::new_async().await;
    let final_hits = Arc::new(Mutex::new(0usize));
    let hits = final_hits.clone();
    server
        .mock("POST", "/v1/chat/completions")
        .with_header("content-type", "text/event-stream")
        .with_body_from_request(move |req| {
            let body: Value =
                serde_json::from_str(&req.utf8_lossy_body().unwrap()).unwrap_or(Value::Null);
            if openai_final(&body) {
                let mut n = hits.lock().unwrap();
                *n += 1;
                if *n == 1 {
                    // A stream that breaks off without a finish or [DONE].
                    return b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"{\\\"fi\"}"
                        .to_vec();
                }
                return openai_sse(&json!({"findings": "42"}).to_string(), "stop").into_bytes();
            }
            openai_sse(PROSE, "stop").into_bytes()
        })
        .create_async()
        .await;
    let orchestrator = orchestrator_on(&pool, openai_router(&server.url()));

    let outcome = run_delegate(&orchestrator)
        .await
        .expect("the retry reports");
    assert_eq!(answer_of(&outcome)["findings"], "42");
    assert_eq!(*final_hits.lock().unwrap(), 2);
}

/// Claude Opus 5.5, Sonnet 5.5 and Fable 5.1 reject a forced `tool_choice`
/// with a 400, which failed every schema'd delegate on them. A host marked
/// `anthropic_thinking: "adaptive"` gets structured outputs instead —
/// `output_config.format = {type: "json_schema", schema}` beside the delegate's
/// effort, with adaptive thinking kept — and the answer comes back as text.
#[tokio::test]
async fn an_adaptive_claude_host_gets_structured_outputs_not_a_forced_tool() {
    let pool = test_pool().await;
    let mut server = mockito::Server::new_async().await;
    let bodies = mock_provider(&mut server, "/v1/messages", anthropic_final, |body, _| {
        if anthropic_final(body) {
            anthropic_sse(&json!({"findings": "42"}).to_string(), "end_turn")
        } else {
            anthropic_sse(PROSE, "end_turn")
        }
    })
    .await;
    let router = Arc::new(ProviderRouter::new(BigTinyConfig::default().cache));
    router.register_anthropic(
        "claude",
        ProviderConfig {
            provider_type: "anthropic".into(),
            base_url: server.url(),
            model: "claude-opus-5-5".into(),
            anthropic_thinking: Some("adaptive".into()),
            delegate_effort: Some("low".into()),
            ..Default::default()
        },
    );
    let orchestrator = orchestrator_on(&pool, router);

    let outcome = run_delegate(&orchestrator)
        .await
        .expect("the delegate reports");
    assert_eq!(answer_of(&outcome)["findings"], "42");

    let bodies = bodies.lock().unwrap().clone();
    for b in &bodies {
        assert_eq!(
            b["output_config"]["effort"], "low",
            "every request runs low: {b}"
        );
        assert!(b.get("tool_choice").is_none(), "never a forced tool: {b}");
    }
    let final_pass = bodies
        .iter()
        .find(|b| anthropic_final(b))
        .expect("a final pass");
    assert_eq!(final_pass["output_config"]["format"]["type"], "json_schema");
    let sent = &final_pass["output_config"]["format"]["schema"];
    assert_eq!(sent["required"], json!(["findings"]));
    assert_eq!(sent["additionalProperties"], false);
    // Narrowed for the wire only; the answer is still validated in full.
    assert!(sent["properties"]["findings"].get("maxLength").is_none());
    assert!(final_pass.get("tools").is_none());
    assert_eq!(final_pass["thinking"]["type"], "adaptive");
    assert_eq!(final_pass["max_tokens"], 8192);
}
