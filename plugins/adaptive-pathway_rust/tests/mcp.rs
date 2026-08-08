//! Phase 4 acceptance: the `PathwayServer` MCP surface is exactly the two
//! write tools `record` + `forget`, and each tool's happy path works against
//! an in-memory engine. The full duplex handshake is exercised end-to-end by
//! bigtiny_rust's `MCPServerClient::connect_in_process` in Phase 5.

use adaptive_pathway::config::Config;
use adaptive_pathway::engine::PathwayEngine;
use adaptive_pathway::mcp::{ForgetKind, ForgetRequest, PathwayServer, RecordRequest};
use adaptive_pathway::store::beliefs::{Layer, Provenance};
use rmcp::handler::server::wrapper::Parameters;

#[tokio::test]
async fn tool_surface_is_record_and_forget() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    let s = PathwayServer::new(engine, "s1".to_string());
    let names = s.tool_names();
    assert_eq!(names, vec!["forget", "record"], "exactly two tools, sorted");
}

#[tokio::test]
async fn record_adds_a_belief() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    let s = PathwayServer::new(engine.clone(), "s1".to_string());

    let resp = s
        .record(Parameters(RecordRequest {
            observation: "The user prefers terse code comments.".to_string(),
            provenance: "direct_statement".to_string(),
            domain_hint: Some("coding".to_string()),
            session_id: None,
        }))
        .await;
    assert!(resp.contains("\"status\":\"ok\"") || resp.contains("\"status\": \"ok\""));

    let beliefs = engine.db.list_beliefs(None).await.unwrap();
    assert_eq!(beliefs.len(), 1);
    assert!(beliefs[0].text.contains("terse code comments"));
    assert_eq!(beliefs[0].layer, Layer::Context);
}

#[tokio::test]
async fn empty_record_returns_soft_message() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    let s = PathwayServer::new(engine, "s1".to_string());
    let resp = s
        .record(Parameters(RecordRequest {
            observation: "   ".to_string(),
            provenance: "inferred_pattern".to_string(),
            domain_hint: None,
            session_id: None,
        }))
        .await;
    assert!(resp.contains("empty_observation"));
}

#[tokio::test]
async fn forget_returns_dropped_text() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    let s = PathwayServer::new(engine.clone(), "s1".to_string());

    adaptive_pathway::belief::synthesis::route_observation(
        &engine.db,
        "The user likes dark mode.",
        &[1.0f32, 0.0, 0.0, 0.0],
        &engine.cfg.embedding.ollama_model,
        Provenance::DirectStatement,
        Layer::Context,
        None,
        None,
        None,
        Some("s1".to_string()),
        None,
        chrono::Utc::now(),
    )
    .await
    .unwrap();

    let resp = s
        .forget(Parameters(ForgetRequest {
            what: "dark mode".to_string(),
            reason: ForgetKind::Wrong,
            session_id: None,
        }))
        .await;
    assert!(resp.contains("Dropped:"), "response should echo dropped text: {resp}");
}

#[tokio::test]
async fn forget_nothing_matches_is_soft() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    let s = PathwayServer::new(engine, "s1".to_string());
    let resp = s
        .forget(Parameters(ForgetRequest {
            what: "something never recorded".to_string(),
            reason: ForgetKind::Private,
            session_id: None,
        }))
        .await;
    assert!(resp.contains("\"status\":\"ok\"") || resp.contains("\"status\": \"ok\""));
}

// --- Host-injected session scope (Workstream D) ------------------------------

#[tokio::test]
async fn an_injected_session_id_overrides_the_construction_time_one() {
    // The daemon's in-process `pathway` connection is built with an empty
    // session id (`mcp::builtin::connect`) because it is shared across
    // concurrently-streaming sessions; `agent::loop_` injects the executing
    // session into the tool arguments instead. Pause is the cleanest
    // observable proof that the injected id -- not the construction-time one
    // -- is what the handler actually scopes to.
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    engine.set_paused("paused-session", true).await.unwrap();
    let s = PathwayServer::new(engine.clone(), String::new());

    let paused = s
        .record(Parameters(RecordRequest {
            observation: "The user prefers terse code comments.".to_string(),
            provenance: "direct_statement".to_string(),
            domain_hint: None,
            session_id: Some("paused-session".to_string()),
        }))
        .await;
    assert!(paused.contains("memory is paused"), "got: {paused}");
    assert!(engine.db.list_beliefs(None).await.unwrap().is_empty());

    let live = s
        .record(Parameters(RecordRequest {
            observation: "The user prefers terse code comments.".to_string(),
            provenance: "direct_statement".to_string(),
            domain_hint: None,
            session_id: Some("live-session".to_string()),
        }))
        .await;
    assert!(live.contains("recorded"), "got: {live}");
    assert_eq!(engine.db.list_beliefs(None).await.unwrap().len(), 1);
}

#[tokio::test]
async fn an_absent_or_empty_injected_session_falls_back_to_the_bound_one() {
    // `devtool serve-stdio` and the tests above construct a server bound to a
    // real session and inject nothing -- that path must keep working.
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    engine.set_paused("s1", true).await.unwrap();
    let s = PathwayServer::new(engine.clone(), "s1".to_string());

    for injected in [None, Some(String::new())] {
        let resp = s
            .record(Parameters(RecordRequest {
                observation: "The user prefers terse code comments.".to_string(),
                provenance: "direct_statement".to_string(),
                domain_hint: None,
                session_id: injected,
            }))
            .await;
        assert!(resp.contains("memory is paused"), "got: {resp}");
    }
    assert!(engine.db.list_beliefs(None).await.unwrap().is_empty());
}
