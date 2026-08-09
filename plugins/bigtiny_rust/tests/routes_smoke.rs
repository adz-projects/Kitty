//! Smoke tests for the route table (Phase D): axum's `{id}` path syntax is
//! only validated when `Router::new().route(...)` actually runs, not at
//! compile time, so this exercises `create_router` for real and hits a
//! representative sample of routes end-to-end against an in-memory DB.

use std::sync::Arc;

use bigtiny_rust::agent::summarizer_chain::SummarizerChain;
use bigtiny_rust::agent::Agent;
use bigtiny_rust::config::BigTinyConfig;
use bigtiny_rust::hitl::manager::HITLManager;
use bigtiny_rust::mcp::MCPManager;
use bigtiny_rust::provider::router::ProviderRouter;
use bigtiny_rust::recipes::engine::RecipeEngine;
use bigtiny_rust::routes::{create_router, AppState};
use bigtiny_rust::scheduler::Scheduler;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tower::ServiceExt;

async fn test_state() -> Arc<AppState> {
    test_state_with_pathway(None).await
}

/// Same as [`test_state`] but with a caller-supplied config, for routes whose
/// behaviour is gated on a config flag (e.g. `[local].enabled`).
#[allow(dead_code)]
async fn test_state_with_config(config: BigTinyConfig) -> Arc<AppState> {
    test_state_inner(None, config).await
}

async fn test_state_with_pathway(
    pathway: Option<Arc<adaptive_pathway::engine::PathwayEngine>>,
) -> Arc<AppState> {
    test_state_inner(pathway, BigTinyConfig::default()).await
}

async fn test_state_inner(
    pathway: Option<Arc<adaptive_pathway::engine::PathwayEngine>>,
    config: BigTinyConfig,
) -> Arc<AppState> {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let router = Arc::new(ProviderRouter::new(config.cache.clone()));
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
        router.clone(),
        mcp.clone(),
        hitl,
        summarizer,
        config.clone(),
        std::env::temp_dir().to_string_lossy().into_owned(),
        None,
        None,
    ));

    let recipe_engine = Arc::new(RecipeEngine::new(
        pool.clone(),
        agent.clone(),
        mcp.clone(),
        std::env::temp_dir(),
    ));
    let scheduler = Arc::new(tokio::sync::Mutex::new(
        Scheduler::new(pool.clone(), recipe_engine.clone())
            .await
            .unwrap(),
    ));

    Arc::new(AppState {
        db: pool,
        agent,
        mcp,
        router,
        recipe_engine,
        scheduler,
        config,
        pathway,
        #[cfg(feature = "local-engine")]
        local_slots: bigtiny_rust::local::SlotManager::new(),
    })
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn router_constructs_without_panicking_and_health_is_open() {
    let state = test_state().await;
    let app = create_router(state);

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/health")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn create_list_and_fetch_session_roundtrip() {
    let state = test_state().await;
    let app = create_router(state);

    let create_resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/chat/")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"cwd": "/tmp", "mode": "chat"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), axum::http::StatusCode::OK);
    let created = body_json(create_resp).await;
    let session_id = created["session_id"].as_str().unwrap().to_string();

    let list_resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/chat/?limit=200")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_resp.status(), axum::http::StatusCode::OK);
    let list = body_json(list_resp).await;
    let sessions = list["sessions"].as_array().unwrap();
    assert!(sessions.iter().any(|s| s["id"] == session_id));
    // `total` used to be entirely absent; `limit`/`offset` were parsed but
    // never applied to the query, so this session count check previously
    // couldn't have failed no matter how badly pagination was broken.
    assert_eq!(list["total"].as_i64().unwrap(), 1);

    let paginated_resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/chat/?limit=0&offset=0")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let paginated = body_json(paginated_resp).await;
    assert_eq!(paginated["sessions"].as_array().unwrap().len(), 0);
    assert_eq!(paginated["total"].as_i64().unwrap(), 1);

    let history_resp = app
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/api/chat/{session_id}/history"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(history_resp.status(), axum::http::StatusCode::OK);
    let history = body_json(history_resp).await;
    assert!(history.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn chat_dir_is_set_at_creation_and_survives_a_later_cwd_repoint() {
    let state = test_state().await;
    let app = create_router(state);

    let create_resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/chat/")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"cwd": "/original/dir", "mode": "agentic"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let created = body_json(create_resp).await;
    let session_id = created["session_id"].as_str().unwrap().to_string();

    // "Set as working directory" repoints cwd in place — chat_dir must stay
    // pointed at the session's original directory so the sandbox keeps
    // allowing it (agent/sandbox.rs::allowed_dirs_for_session).
    let patch_resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("PATCH")
                .uri(format!("/api/chat/{session_id}/config"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"cwd": "/new/dir"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(patch_resp.status(), axum::http::StatusCode::OK);

    let list_resp = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/chat/?limit=200")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list = body_json(list_resp).await;
    let sessions = list["sessions"].as_array().unwrap();
    let row = sessions.iter().find(|s| s["id"] == session_id).unwrap();
    let metadata: Value = serde_json::from_str(row["metadata"].as_str().unwrap()).unwrap();
    assert_eq!(metadata["chat_dir"], "/original/dir");
    assert_eq!(metadata["cwd"], "/new/dir");
}

#[tokio::test]
async fn create_session_defaults_mode_to_chat_not_null() {
    let state = test_state().await;
    let app = create_router(state);

    let create_resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/chat/")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json!({}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let created = body_json(create_resp).await;
    let session_id = created["session_id"].as_str().unwrap().to_string();

    let list_resp = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/chat/?limit=200")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list = body_json(list_resp).await;
    let sessions = list["sessions"].as_array().unwrap();
    let row = sessions.iter().find(|s| s["id"] == session_id).unwrap();
    let metadata: Value = serde_json::from_str(row["metadata"].as_str().unwrap()).unwrap();
    assert_eq!(metadata["mode"], "chat");
}

#[tokio::test]
async fn mcp_and_providers_and_recipes_and_schedules_list_endpoints_respond() {
    let state = test_state().await;
    let app = create_router(state);

    for path in [
        "/api/mcp/servers",
        "/api/providers",
        "/api/recipes",
        "/api/schedules",
    ] {
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::OK,
            "path {path} failed"
        );
    }
}

#[tokio::test]
async fn send_on_a_nonexistent_session_returns_not_found() {
    let state = test_state().await;
    let app = create_router(state);

    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/chat/does-not-exist/send")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json!({"message": "hi"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cancel_on_an_idle_session_is_a_no_op_and_marks_it_idle() {
    let state = test_state().await;
    let app = create_router(state.clone());

    let create_resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/chat/")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json!({}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let session_id = body_json(create_resp).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let cancel_resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/api/chat/{session_id}/cancel"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cancel_resp.status(), axum::http::StatusCode::OK);
    assert_eq!(body_json(cancel_resp).await["ok"], true);

    let status: String = sqlx::query_scalar("SELECT status FROM sessions WHERE id = ?")
        .bind(&session_id)
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(status, "idle");
}

#[tokio::test]
async fn approve_with_an_unknown_action_id_is_rejected_not_a_server_error() {
    let state = test_state().await;
    let app = create_router(state);

    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/chat/some-session/approve")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"action_id": "does-not-exist", "decision": "allow"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let decision = body_json(resp).await;
    assert_eq!(decision["action"], "rejected");
}

#[tokio::test]
async fn fork_remaps_the_compaction_boundary_to_the_new_sessions_own_rowids() {
    let state = test_state().await;
    let app = create_router(state.clone());

    let create_resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/chat/")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json!({}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let session_id = body_json(create_resp).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    for (role, content) in [("user", "msg1"), ("assistant", "msg2"), ("user", "msg3")] {
        sqlx::query("INSERT INTO messages (id, session_id, role, content) VALUES (?, ?, ?, ?)")
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(&session_id)
            .bind(role)
            .bind(content)
            .execute(&state.db)
            .await
            .unwrap();
    }
    let boundary_rowid: i64 =
        sqlx::query_scalar("SELECT rowid FROM messages WHERE session_id = ? AND content = 'msg2'")
            .bind(&session_id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    bigtiny_rust::storage::sessions::update_compaction_state(
        &state.db,
        &session_id,
        "a summary of msg1",
        boundary_rowid,
    )
    .await
    .unwrap();

    let fork_resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/api/chat/{session_id}/fork"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json!({}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(fork_resp.status(), axum::http::StatusCode::OK);
    let new_id = body_json(fork_resp).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let new_boundary_rowid: i64 =
        sqlx::query_scalar("SELECT compacted_through_rowid FROM sessions WHERE id = ?")
            .bind(&new_id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    let new_msg2_rowid: i64 =
        sqlx::query_scalar("SELECT rowid FROM messages WHERE session_id = ? AND content = 'msg2'")
            .bind(&new_id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(new_boundary_rowid, new_msg2_rowid);
    // Not the source session's original rowid — that would point at the
    // wrong message (or nothing) in the new session's own rows.
    assert_ne!(new_boundary_rowid, boundary_rowid);

    let memory_slots: Option<String> =
        sqlx::query_scalar("SELECT memory_slots FROM sessions WHERE id = ?")
            .bind(&new_id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(memory_slots.as_deref(), Some("a summary of msg1"));
}

#[tokio::test]
async fn create_recipe_then_execute_it_creates_a_session() {
    let state = test_state().await;
    let app = create_router(state);

    let create_resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/recipes")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"name": "smoke test recipe", "prompt_template": "Say hi"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), axum::http::StatusCode::OK);
    let recipe_id = body_json(create_resp).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let exec_resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/api/recipes/{recipe_id}/execute"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json!({}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(exec_resp.status(), axum::http::StatusCode::OK);
    let executed = body_json(exec_resp).await;
    assert!(executed["session_id"].as_str().is_some());
}

#[tokio::test]
async fn create_schedule_then_run_now_executes_its_recipe() {
    let state = test_state().await;
    let app = create_router(state.clone());

    let recipe_resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/recipes")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"name": "scheduled recipe", "prompt_template": "Say hi"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let recipe_id = body_json(recipe_resp).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    // `enabled: false` skips live cron registration (no need to get a valid
    // cron expression right for this test) while still exercising storage +
    // `run_now`, which doesn't check the enabled flag.
    let schedule_resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/schedules")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({
                        "name": "nightly",
                        "cron": "0 0 * * *",
                        "recipe_id": recipe_id,
                        "enabled": false
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(schedule_resp.status(), axum::http::StatusCode::OK);
    let schedule_id = body_json(schedule_resp).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let run_resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/api/schedules/{schedule_id}/run_now"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(run_resp.status(), axum::http::StatusCode::OK);
    assert_eq!(body_json(run_resp).await["ok"], true);

    // execute_job (fired by run_now) creates a temp session and an
    // execution_history row for it — confirms the recipe actually ran
    // through the scheduler path, not just that the HTTP call returned ok.
    let execution_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM execution_history WHERE trigger_type = 'schedule'",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(execution_count, 1);
}

#[tokio::test]
async fn provider_api_key_is_encrypted_at_rest_and_never_echoed_over_http() {
    let state = test_state().await;
    let app = create_router(state.clone());
    let plaintext_key = "sk-super-secret-do-not-leak";

    let create_resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/providers")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({
                        "name": "test-provider",
                        "provider_type": "openai_compat",
                        "base_url": "http://127.0.0.1:1",
                        "api_key": plaintext_key
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), axum::http::StatusCode::OK);
    let provider_id = body_json(create_resp).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    // At rest: the DB column must not contain the plaintext key.
    let stored_config: String = sqlx::query_scalar("SELECT config FROM providers WHERE id = ?")
        .bind(&provider_id)
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert!(!stored_config.contains(plaintext_key));
    assert!(stored_config.contains("enc:v1:"));

    // Over HTTP: neither the list nor a PATCH response echoes it back.
    let list_resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/providers")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list_bytes = list_resp.into_body().collect().await.unwrap().to_bytes();
    let list_str = String::from_utf8(list_bytes.to_vec()).unwrap();
    assert!(!list_str.contains(plaintext_key));
    let list: Value = serde_json::from_str(&list_str).unwrap();
    let listed = list["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == provider_id)
        .unwrap();
    assert_eq!(listed["has_api_key"], true);

    let patch_resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("PATCH")
                .uri(format!("/api/providers/{provider_id}"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"name": "renamed"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let patch_bytes = patch_resp.into_body().collect().await.unwrap().to_bytes();
    let patch_str = String::from_utf8(patch_bytes.to_vec()).unwrap();
    assert!(!patch_str.contains(plaintext_key));
    let patched: Value = serde_json::from_str(&patch_str).unwrap();
    assert_eq!(patched["has_api_key"], true);

    // Still usable internally — the router decrypts it back for real use.
    assert_eq!(
        bigtiny_rust::crypto::decrypt(
            serde_json::from_str::<Value>(&stored_config).unwrap()["api_key"]
                .as_str()
                .unwrap()
        ),
        plaintext_key
    );
}

#[tokio::test]
async fn mcp_server_header_value_is_encrypted_at_rest_and_never_echoed_over_http() {
    let state = test_state().await;
    let app = create_router(state.clone());
    let plaintext_key = "Bearer sk-mcp-secret-do-not-leak";

    let create_resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/mcp/servers")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({
                        "name": "test-server",
                        "transport": "streamable_http",
                        "url": "http://127.0.0.1:1",
                        "headers": {"Authorization": plaintext_key}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), axum::http::StatusCode::OK);
    let server_id = body_json(create_resp).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let stored_headers: String = sqlx::query_scalar("SELECT headers FROM mcp_servers WHERE id = ?")
        .bind(&server_id)
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert!(!stored_headers.contains(plaintext_key));
    assert!(stored_headers.contains("enc:v1:"));

    let list_resp = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/mcp/servers")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list_bytes = list_resp.into_body().collect().await.unwrap().to_bytes();
    let list_str = String::from_utf8(list_bytes.to_vec()).unwrap();
    assert!(!list_str.contains(plaintext_key));
    let list: Value = serde_json::from_str(&list_str).unwrap();
    let listed = list["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == server_id)
        .unwrap();
    let listed_headers: Value = serde_json::from_str(listed["headers"].as_str().unwrap()).unwrap();
    assert_eq!(listed_headers["Authorization"], "***");

    // Still usable internally.
    let stored_headers_json: Value = serde_json::from_str(&stored_headers).unwrap();
    assert_eq!(
        bigtiny_rust::crypto::decrypt(stored_headers_json["Authorization"].as_str().unwrap()),
        plaintext_key
    );
}

#[tokio::test]
async fn pathway_routes_are_disabled_gracefully_with_no_engine() {
    let state = test_state().await; // pathway: None
    let app = create_router(state);
    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/pathway/beliefs")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // Disabled pathway responds 200 with a soft `{"error": ...}` body (not a
    // 5xx), matching `memory::stats`'s "never fail the daemon" shape.
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body.get("error").is_some());
}

/// `/api/embeddings` must exist on every build so the wire contract is
/// stable — a 404 would be indistinguishable from a wrong URL, and Phase 2b
/// re-points adaptive-pathway at exactly this path. With `[local].enabled`
/// false (the default) it reports 503 + an explanation rather than pretending
/// to work.
#[tokio::test]
async fn embeddings_route_exists_and_explains_itself_when_disabled() {
    let state = test_state().await;
    let app = create_router(state);
    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/embeddings")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(r#"{"prompt":"hello"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "disabled local engine should be 503, not 404 or 500"
    );
    let body = body_json(resp).await;
    assert!(
        body.get("error").is_some(),
        "a 503 must say why: {body:?}"
    );
}

/// A malformed request is the caller's fault, not the engine's, and must be
/// distinguishable from "engine unavailable" — but only once the engine is
/// actually enabled, since the disabled check short-circuits first.
#[cfg(feature = "local-engine")]
#[tokio::test]
async fn embeddings_route_rejects_a_missing_prompt_as_a_bad_request() {
    let cfg = BigTinyConfig {
        local: bigtiny_rust::config::LocalEngineConfig {
            enabled: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let app = create_router(test_state_with_config(cfg).await);
    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/embeddings")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(r#"{"model":"x"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
}

/// End-to-end through the real route with a real GGUF, when one is available.
///
/// Opt-in via `KITTY_TEST_EMBED_GGUF=<path>` rather than skipped-by-default
/// magic: the model is ~376 MB and not in the repo, so CI can't run this, but
/// it is the only test that proves the wire contract Phase 2b depends on
/// (`{prompt}` → `{embedding:[...]}`) actually holds against the engine.
#[cfg(feature = "local-engine")]
#[tokio::test]
async fn embeddings_route_returns_a_real_vector_when_a_model_is_available() {
    let Ok(path) = std::env::var("KITTY_TEST_EMBED_GGUF") else {
        eprintln!("skipping: set KITTY_TEST_EMBED_GGUF to a GGUF path to run");
        return;
    };
    let cfg = BigTinyConfig {
        local: bigtiny_rust::config::LocalEngineConfig {
            enabled: true,
            embed_model_path: path,
            ..Default::default()
        },
        ..Default::default()
    };
    let app = create_router(test_state_with_config(cfg).await);
    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/embeddings")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    r#"{"model":"embed","prompt":"the cat sat on the mat"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = body_json(resp).await;
    let v = body
        .get("embedding")
        .and_then(|v| v.as_array())
        .expect("response must carry an `embedding` array — Ollama's shape");
    assert!(v.len() > 100, "expected a real vector, got {} dims", v.len());
    // Guard the failure mode a shape check would miss: a backend that returns
    // a correctly-sized block of zeros/NaNs looks fine here but destroys
    // adaptive-pathway recall.
    let floats: Vec<f64> = v.iter().filter_map(|x| x.as_f64()).collect();
    assert_eq!(floats.len(), v.len(), "all entries must be numbers");
    assert!(floats.iter().all(|x| x.is_finite()), "no NaN/inf allowed");
    assert!(
        floats.iter().any(|x| x.abs() > 1e-6),
        "all-zero embedding is not a usable vector"
    );
    let norm: f64 = floats.iter().map(|x| x * x).sum::<f64>().sqrt();
    assert!((norm - 1.0).abs() < 1e-3, "expected L2-normalised, got {norm}");
}

#[tokio::test]
async fn pathway_beliefs_list_and_delete_round_trip() {
    let engine = adaptive_pathway::engine::PathwayEngine::open_in_memory(
        adaptive_pathway::config::Config::default(),
    )
    .await
    .unwrap();

    // Seed one belief directly through the engine's store.
    let now = chrono::Utc::now();
    engine
        .db
        .insert_belief(&adaptive_pathway::store::beliefs::Belief {
            id: "b1".into(),
            text: "The user prefers terse code comments.".into(),
            embedding: vec![1.0, 0.0],
            confidence: 0.7,
            provenance: adaptive_pathway::store::beliefs::Provenance::DirectStatement,
            layer: adaptive_pathway::store::beliefs::Layer::Context,
            tested: true,
            domain: None,
            tier: "context".into(),
            support_count: 1,
            distinct_sessions: 1,
            contradict_count: 0,
            pinned: false,
            last_confirmed_at: Some(now),
            consolidated_at: None,
            created_at: now,
            updated_at: now,
            session_id: None,
            embedding_model: adaptive_pathway::config::Config::default().embedding.ollama_model,
        })
        .await
        .unwrap();

    let state = test_state_with_pathway(Some(engine)).await;
    let app = create_router(state);

    let list_resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/pathway/beliefs")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_resp.status(), axum::http::StatusCode::OK);
    let list_body = body_json(list_resp).await;
    assert_eq!(list_body["count"], 1);
    assert_eq!(list_body["beliefs"][0]["id"], "b1");

    let delete_resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("DELETE")
                .uri("/api/pathway/beliefs/b1")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(delete_resp.status(), axum::http::StatusCode::OK);
    let delete_body = body_json(delete_resp).await;
    assert_eq!(delete_body["dropped"], "The user prefers terse code comments.");

    // A permanent ("wrong") suppression doesn't hard-delete the row, but the
    // belief browser's list should reflect it as gone -- listing every
    // belief regardless of suppression isn't what the browser wants, so this
    // documents the current (pre-#7) behavior: `list_beliefs` has no
    // suppression filter yet, matching issue #11's still-open status.
    let relist_resp = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/pathway/beliefs")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let relist_body = body_json(relist_resp).await;
    assert_eq!(
        relist_body["count"], 1,
        "list_beliefs has no suppression filter yet (issue #11) -- the suppressed belief still lists"
    );
}

#[tokio::test]
async fn pathway_delete_unknown_id_returns_soft_error() {
    let engine = adaptive_pathway::engine::PathwayEngine::open_in_memory(
        adaptive_pathway::config::Config::default(),
    )
    .await
    .unwrap();
    let state = test_state_with_pathway(Some(engine)).await;
    let app = create_router(state);

    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("DELETE")
                .uri("/api/pathway/beliefs/does-not-exist")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body.get("error").is_some());
}

#[tokio::test]
async fn pathway_set_paused_round_trip() {
    let engine = adaptive_pathway::engine::PathwayEngine::open_in_memory(
        adaptive_pathway::config::Config::default(),
    )
    .await
    .unwrap();
    let state = test_state_with_pathway(Some(engine)).await;
    let app = create_router(state);

    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("PATCH")
                .uri("/api/pathway/sessions/s1/pause")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(json!({"paused": true}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["session_id"], "s1");
    assert_eq!(body["paused"], true);
}
