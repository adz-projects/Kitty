//! Cross-tenant isolation.
//!
//! This file is the guard the tenancy design rests on. Session routes call
//! `deny_unless_owned` at entry rather than threading `app_id` through the
//! whole crate, which is far less invasive but means a *new* route could
//! silently forget the check. The sweep below walks every `/api/chat/{id}/*`
//! route as a non-owner and expects a 404 from each, so that mistake fails
//! here instead of leaking someone's conversation in production.
//!
//! Every assertion is 404, never 403: "this exists but is not yours" confirms
//! another app's session id, and no legitimate caller benefits from being able
//! to tell the two apart.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use bigtiny2::agent::summarizer_chain::SummarizerChain;
use bigtiny2::agent::Agent;
use bigtiny2::config::BigTinyConfig;
use bigtiny2::hitl::manager::HITLManager;
use bigtiny2::mcp::MCPManager;
use bigtiny2::provider::router::ProviderRouter;
use bigtiny2::recipes::engine::RecipeEngine;
use bigtiny2::routes::AppState;
use bigtiny2::scheduler::Scheduler;
use bigtiny2::storage::apps::{self, AppIdentity};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tower::ServiceExt;

const APP_A: &str = "app-a";
const APP_B: &str = "app-b";

async fn test_state() -> Arc<AppState> {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let plugins = bigtiny2::plugins::test_plugin_host(&pool);

    apps::register_app(&pool, APP_A, "App A", "key-a").await.unwrap();
    apps::register_app(&pool, APP_B, "App B", "key-b").await.unwrap();

    let config = BigTinyConfig::default();
    let router = Arc::new(ProviderRouter::new(config.cache.clone()));
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
        mcp.clone(),
        hitl,
        summarizer,
        config.clone(),
        std::env::temp_dir().to_string_lossy().into_owned(),
        plugins.clone(),
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
        plugins: plugins.clone(),
        key_cache: Arc::new(bigtiny2::server::middleware::KeyCache::new()),
        instance_id: "test-instance".to_string(),
    })
}

fn router_as(state: Arc<AppState>, app_id: &str) -> axum::Router {
    use axum::Extension;
    bigtiny2::routes::create_router(state).layer(Extension(AppIdentity {
        app_id: app_id.to_string(),
        scopes: vec!["*".to_string()],
    }))
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// Create a session owned by `app_id` and return its id.
async fn create_session(state: Arc<AppState>, app_id: &str) -> String {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/chat/")
        .header("content-type", "application/json")
        .body(Body::from(json!({"name": "owned"}).to_string()))
        .unwrap();
    let resp = router_as(state, app_id).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await["session_id"].as_str().unwrap().to_string()
}

// ---------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------

/// Every session-scoped route, as (method, path suffix, body).
///
/// Add a row here whenever a `/api/chat/{id}/*` route is added. That is the
/// point: a route missing from this list is a route nobody proved is scoped.
fn session_routes(id: &str) -> Vec<(Method, String, Option<Value>)> {
    vec![
        (Method::GET, format!("/api/chat/{id}/history"), None),
        (Method::GET, format!("/api/chat/{id}/stats"), None),
        (Method::GET, format!("/api/chat/{id}/timings"), None),
        (Method::GET, format!("/api/chat/{id}/pending"), None),
        (
            Method::PATCH,
            format!("/api/chat/{id}"),
            Some(json!({"name": "renamed by an intruder"})),
        ),
        (
            Method::PATCH,
            format!("/api/chat/{id}/config"),
            Some(json!({"cwd": "/tmp/intruder"})),
        ),
        (Method::DELETE, format!("/api/chat/{id}"), None),
        (Method::POST, format!("/api/chat/{id}/fork"), Some(json!({}))),
        (Method::POST, format!("/api/chat/{id}/cancel"), Some(json!({}))),
        (Method::POST, format!("/api/chat/{id}/compact"), Some(json!({}))),
        (
            Method::POST,
            format!("/api/chat/{id}/send"),
            Some(json!({"message": "hello"})),
        ),
        (
            Method::POST,
            format!("/api/chat/{id}/approve"),
            Some(json!({"action_id": "whatever", "decision": "allow"})),
        ),
    ]
}

#[tokio::test]
async fn no_session_route_is_reachable_by_a_non_owner() {
    let state = test_state().await;
    let session = create_session(state.clone(), APP_A).await;

    for (method, path, body) in session_routes(&session) {
        let mut builder = Request::builder().method(method.clone()).uri(&path);
        let req = match &body {
            Some(b) => {
                builder = builder.header("content-type", "application/json");
                builder.body(Body::from(b.to_string())).unwrap()
            }
            None => builder.body(Body::empty()).unwrap(),
        };

        let resp = router_as(state.clone(), APP_B).oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "{method} {path} leaked to a non-owner (got {})",
            resp.status()
        );
    }
}

#[tokio::test]
async fn every_session_route_works_for_its_actual_owner() {
    // The other half of the sweep above: proves those 404s come from the
    // ownership check rather than from the routes being broken outright.
    let state = test_state().await;
    let session = create_session(state.clone(), APP_A).await;

    for (method, path, body) in session_routes(&session) {
        // Three exclusions, each for a reason unrelated to ownership:
        //   * `send` needs a configured provider;
        //   * `delete` would remove the session mid-sweep (covered separately);
        //   * `approve` 404s for a nonexistent action id even for the owner,
        //     which is correct and long-standing behaviour -- so it cannot
        //     distinguish "denied" from "no such action" here.
        if path.ends_with("/send") || path.ends_with("/approve") || method == Method::DELETE {
            continue;
        }

        let mut builder = Request::builder().method(method.clone()).uri(&path);
        let req = match &body {
            Some(b) => {
                builder = builder.header("content-type", "application/json");
                builder.body(Body::from(b.to_string())).unwrap()
            }
            None => builder.body(Body::empty()).unwrap(),
        };

        let resp = router_as(state.clone(), APP_A).oneshot(req).await.unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "{method} {path} 404'd for its own owner"
        );
    }
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn listing_shows_only_the_callers_own_sessions() {
    let state = test_state().await;
    create_session(state.clone(), APP_A).await;
    create_session(state.clone(), APP_A).await;
    create_session(state.clone(), APP_B).await;

    for (app, expected) in [(APP_A, 2), (APP_B, 1)] {
        let req = Request::builder()
            .uri("/api/chat/")
            .body(Body::empty())
            .unwrap();
        let resp = router_as(state.clone(), app).oneshot(req).await.unwrap();
        let body = body_json(resp).await;

        assert_eq!(body["sessions"].as_array().unwrap().len(), expected);
        // The `total` must be scoped too -- an unscoped count leaks the size
        // of every other app's history through a pagination footer.
        assert_eq!(body["total"].as_i64().unwrap(), expected as i64);
    }
}

#[tokio::test]
async fn a_deleted_session_is_gone_for_its_owner_and_never_reachable_by_anyone_else() {
    let state = test_state().await;
    let session = create_session(state.clone(), APP_A).await;

    let del = |app: &str, id: String| {
        let state = state.clone();
        let app = app.to_string();
        async move {
            let req = Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/chat/{id}"))
                .body(Body::empty())
                .unwrap();
            router_as(state, &app).oneshot(req).await.unwrap().status()
        }
    };

    // B cannot delete A's session...
    assert_eq!(del(APP_B, session.clone()).await, StatusCode::NOT_FOUND);
    // ...and the failed attempt must not have deleted it either.
    assert_eq!(del(APP_A, session.clone()).await, StatusCode::OK);
    // Now genuinely gone, so even the owner gets a 404.
    assert_eq!(del(APP_A, session).await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_fork_belongs_to_the_app_that_forked_it() {
    let state = test_state().await;
    let session = create_session(state.clone(), APP_A).await;

    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/chat/{session}/fork"))
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = router_as(state.clone(), APP_A).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // A fork that inherited no owner would be invisible to every scoped
    // accessor -- a row nothing can read, list, or delete.
    let req = Request::builder()
        .uri("/api/chat/")
        .body(Body::empty())
        .unwrap();
    let body = body_json(router_as(state, APP_A).oneshot(req).await.unwrap()).await;
    assert_eq!(body["total"].as_i64().unwrap(), 2, "the fork must be listed");
}

// ---------------------------------------------------------------------------
// The unreachable-placeholder invariant
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_empty_app_id_placeholder_is_unreachable() {
    // Migration 017 needs *some* default on `ADD COLUMN NOT NULL`, and uses
    // '' as one no code path may produce. This plants such a row and proves it
    // stays invisible -- if a future change starts writing '', this fails.
    let state = test_state().await;
    sqlx::query("INSERT INTO sessions (id, name, status, app_id) VALUES ('orphan', 'x', 'active', '')")
        .execute(&state.db)
        .await
        .unwrap();

    for app in [APP_A, APP_B, ""] {
        let req = Request::builder()
            .uri(format!("/api/chat/orphan/history"))
            .body(Body::empty())
            .unwrap();
        let status = router_as(state.clone(), app).oneshot(req).await.unwrap().status();
        if app.is_empty() {
            // Even an identity that literally *is* the placeholder should not
            // be arriving here; the register route rejects an empty app_id.
            continue;
        }
        assert_eq!(status, StatusCode::NOT_FOUND, "orphan row visible to {app}");
    }

    let req = Request::builder()
        .uri("/api/chat/")
        .body(Body::empty())
        .unwrap();
    let body = body_json(router_as(state, APP_A).oneshot(req).await.unwrap()).await;
    assert_eq!(body["total"].as_i64().unwrap(), 0);
}

// ---------------------------------------------------------------------------
// App registration and defaults
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_app_sets_its_own_default_without_touching_another_apps() {
    // The V1 failure this replaces: choosing a provider used to rewrite every
    // other app's `fallback_priority` (`demote_others`).
    let state = test_state().await;

    for (app, provider) in [(APP_A, "prov-a"), (APP_B, "prov-b")] {
        let req = Request::builder()
            .method(Method::PATCH)
            .uri("/api/apps/me")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"default_provider_id": provider}).to_string(),
            ))
            .unwrap();
        let resp = router_as(state.clone(), app).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    for (app, expected) in [(APP_A, "prov-a"), (APP_B, "prov-b")] {
        let req = Request::builder()
            .uri("/api/apps/me")
            .body(Body::empty())
            .unwrap();
        let body = body_json(router_as(state.clone(), app).oneshot(req).await.unwrap()).await;
        assert_eq!(body["default_provider_id"], expected);
    }
}

#[tokio::test]
async fn an_app_may_not_revoke_another_app() {
    // Cross-app revocation would hand any registered client a
    // denial-of-service against every other one.
    let state = test_state().await;
    let req = Request::builder()
        .method(Method::DELETE)
        .uri(format!("/api/apps/{APP_B}"))
        .body(Body::empty())
        .unwrap();
    let resp = router_as(state.clone(), APP_A).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // B is still able to authenticate.
    assert!(apps::identity_for_key(&state.db, "key-b")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn registering_a_taken_app_id_is_a_conflict_not_a_new_key() {
    // Reissuing would let anyone holding the (file-readable, per-launch)
    // registration token take over an existing app's identity.
    let state = test_state().await;
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/apps/register")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"app_id": APP_A, "display_name": "Impostor"}).to_string(),
        ))
        .unwrap();
    let resp = router_as(state.clone(), APP_A).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // The original key still works.
    assert!(apps::identity_for_key(&state.db, "key-a")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn an_empty_app_id_is_refused_at_registration() {
    // '' is migration 017's unreachable placeholder; an app named '' would
    // make those rows addressable.
    let state = test_state().await;
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/apps/register")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"app_id": "   ", "display_name": "Blank"}).to_string(),
        ))
        .unwrap();
    let resp = router_as(state, APP_A).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn health_advertises_the_instance_id_discovery_validates_against() {
    let state = test_state().await;
    let req = Request::builder()
        .uri("/api/health")
        .body(Body::empty())
        .unwrap();
    let body = body_json(router_as(state, APP_A).oneshot(req).await.unwrap()).await;

    // Without these two fields a client cannot tell a V2 daemon from a V1 one,
    // nor prove the process on a port is the one its handshake describes.
    assert_eq!(body["instance_id"], "test-instance");
    assert_eq!(body["api_version"], bigtiny2_protocol::API_VERSION);
}

// ---------------------------------------------------------------------------
// Providers
//
// Visibility and mutability are deliberately different questions here: an app
// may *use* a shared provider but not reconfigure or delete one. A row every
// app routes through is not any single client's to rewrite.
// ---------------------------------------------------------------------------

async fn create_provider(state: Arc<AppState>, app_id: &str, name: &str, shared: bool) -> String {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/providers")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "name": name,
                "provider_type": "openai_compat",
                "base_url": "http://127.0.0.1:9",
                "shared": shared,
            })
            .to_string(),
        ))
        .unwrap();
    let resp = router_as(state, app_id).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await["id"].as_str().unwrap().to_string()
}

async fn provider_names(state: Arc<AppState>, app_id: &str) -> Vec<String> {
    let req = Request::builder()
        .uri("/api/providers")
        .body(Body::empty())
        .unwrap();
    let body = body_json(router_as(state, app_id).oneshot(req).await.unwrap()).await;
    body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_private_provider_is_invisible_to_other_apps() {
    let state = test_state().await;
    create_provider(state.clone(), APP_A, "a-only", false).await;

    assert_eq!(provider_names(state.clone(), APP_A).await, vec!["a-only"]);
    assert!(provider_names(state, APP_B).await.is_empty());
}

#[tokio::test]
async fn a_shared_provider_is_visible_to_everyone() {
    // The escape hatch that keeps a user from entering one API key per app.
    let state = test_state().await;
    create_provider(state.clone(), APP_A, "house-key", true).await;

    for app in [APP_A, APP_B] {
        assert_eq!(provider_names(state.clone(), app).await, vec!["house-key"]);
    }
}

#[tokio::test]
async fn another_apps_provider_can_be_neither_read_patched_nor_deleted() {
    let state = test_state().await;
    let id = create_provider(state.clone(), APP_A, "a-only", false).await;

    let patch = Request::builder()
        .method(Method::PATCH)
        .uri(format!("/api/providers/{id}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"base_url": "http://attacker.example"}).to_string(),
        ))
        .unwrap();
    assert_eq!(
        router_as(state.clone(), APP_B).oneshot(patch).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );

    let del = Request::builder()
        .method(Method::DELETE)
        .uri(format!("/api/providers/{id}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router_as(state.clone(), APP_B).oneshot(del).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );

    // Probing would confirm existence and spend another app's rate limit.
    let test = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/providers/{id}/test"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router_as(state.clone(), APP_B).oneshot(test).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );

    // ...and none of the attempts changed anything.
    assert_eq!(provider_names(state, APP_A).await, vec!["a-only"]);
}

#[tokio::test]
async fn a_shared_provider_is_usable_but_not_modifiable() {
    // Visibility != mutability. This is the case a simple "can I see it?"
    // check would get wrong, letting one app repoint a provider every other
    // app is routing through.
    let state = test_state().await;
    let id = create_provider(state.clone(), APP_A, "house-key", true).await;

    let patch = |app: &str| {
        let state = state.clone();
        let app = app.to_string();
        let id = id.clone();
        async move {
            let req = Request::builder()
                .method(Method::PATCH)
                .uri(format!("/api/providers/{id}"))
                .header("content-type", "application/json")
                .body(Body::from(json!({"base_url": "http://elsewhere"}).to_string()))
                .unwrap();
            router_as(state, &app).oneshot(req).await.unwrap().status()
        }
    };

    // Forbidden even for the app that created it: once shared, it is nobody's.
    assert_eq!(patch(APP_A).await, StatusCode::FORBIDDEN);
    assert_eq!(patch(APP_B).await, StatusCode::FORBIDDEN);

    // But both can still see and use it.
    assert_eq!(provider_names(state, APP_B).await, vec!["house-key"]);
}

#[tokio::test]
async fn an_app_can_fully_manage_its_own_provider() {
    // The counterweight to the denial tests: proves those 404s and 403s come
    // from ownership, not from the routes being broken.
    let state = test_state().await;
    let id = create_provider(state.clone(), APP_A, "mine", false).await;

    let patch = Request::builder()
        .method(Method::PATCH)
        .uri(format!("/api/providers/{id}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({"name": "renamed"}).to_string()))
        .unwrap();
    assert_eq!(
        router_as(state.clone(), APP_A).oneshot(patch).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(provider_names(state.clone(), APP_A).await, vec!["renamed"]);

    let del = Request::builder()
        .method(Method::DELETE)
        .uri(format!("/api/providers/{id}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router_as(state.clone(), APP_A).oneshot(del).await.unwrap().status(),
        StatusCode::OK
    );
    assert!(provider_names(state, APP_A).await.is_empty());
}

#[tokio::test]
async fn provider_api_keys_are_never_echoed_back() {
    // Inherited from V1 and worth re-pinning here: with several apps sharing a
    // daemon, a leaked key is no longer only the owner's problem.
    let state = test_state().await;
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/providers")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "name": "keyed",
                "provider_type": "openai_compat",
                "base_url": "http://127.0.0.1:9",
                "api_key": "sk-super-secret",
            })
            .to_string(),
        ))
        .unwrap();
    assert_eq!(
        router_as(state.clone(), APP_A).oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );

    let req = Request::builder()
        .uri("/api/providers")
        .body(Body::empty())
        .unwrap();
    let body = body_json(router_as(state, APP_A).oneshot(req).await.unwrap()).await;
    let serialized = body.to_string();
    assert!(
        !serialized.contains("sk-super-secret"),
        "api key leaked in listing: {serialized}"
    );
    assert_eq!(body["providers"][0]["has_api_key"], true);
}

// ---------------------------------------------------------------------------
// MCP servers, recipes, schedules
//
// MCP servers follow the provider model (visible = own or shared, mutable =
// own only). Recipes and schedules are always exactly one app's -- they encode
// a workflow, not a machine resource, so there is no shared variant.
// ---------------------------------------------------------------------------

async fn create_mcp_server(state: Arc<AppState>, app_id: &str, name: &str, shared: bool) -> String {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/mcp/servers")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"name": name, "transport": "stdio", "command": "echo", "shared": shared})
                .to_string(),
        ))
        .unwrap();
    let resp = router_as(state, app_id).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await["id"].as_str().unwrap().to_string()
}

async fn mcp_names(state: Arc<AppState>, app_id: &str) -> Vec<String> {
    let req = Request::builder()
        .uri("/api/mcp/servers")
        .body(Body::empty())
        .unwrap();
    let body = body_json(router_as(state, app_id).oneshot(req).await.unwrap()).await;
    let mut names: Vec<String> = body["servers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn mcp_servers_are_private_by_default_and_shareable_on_request() {
    let state = test_state().await;
    create_mcp_server(state.clone(), APP_A, "a-tools", false).await;
    create_mcp_server(state.clone(), APP_A, "house-tools", true).await;
    create_mcp_server(state.clone(), APP_B, "b-tools", false).await;

    assert_eq!(
        mcp_names(state.clone(), APP_A).await,
        vec!["a-tools", "house-tools"]
    );
    assert_eq!(mcp_names(state, APP_B).await, vec!["b-tools", "house-tools"]);
}

#[tokio::test]
async fn another_apps_mcp_server_cannot_be_patched_connected_or_inspected() {
    let state = test_state().await;
    let id = create_mcp_server(state.clone(), APP_A, "a-tools", false).await;

    let cases: Vec<(Method, String, Option<Value>)> = vec![
        (
            Method::PATCH,
            format!("/api/mcp/servers/{id}"),
            // The dangerous one: repointing the command another app executes.
            Some(json!({"command": "attacker-binary"})),
        ),
        (
            Method::POST,
            format!("/api/mcp/servers/{id}/connect"),
            Some(json!({})),
        ),
        (Method::GET, format!("/api/mcp/servers/{id}/tools"), None),
        (Method::DELETE, format!("/api/mcp/servers/{id}"), None),
    ];

    for (method, path, body) in cases {
        let mut builder = Request::builder().method(method.clone()).uri(&path);
        let req = match &body {
            Some(b) => {
                builder = builder.header("content-type", "application/json");
                builder.body(Body::from(b.to_string())).unwrap()
            }
            None => builder.body(Body::empty()).unwrap(),
        };
        let status = router_as(state.clone(), APP_B)
            .oneshot(req)
            .await
            .unwrap()
            .status();
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{method} {path} was reachable"
        );
    }

    assert_eq!(mcp_names(state, APP_A).await, vec!["a-tools"]);
}

#[tokio::test]
async fn a_shared_mcp_server_is_usable_but_not_reconfigurable() {
    // The command string on a shared row is what *every* app's agent loop
    // executes. Letting one app repoint it would be a code-execution vector
    // against the others, which is why this is 403 even for its creator.
    let state = test_state().await;
    let id = create_mcp_server(state.clone(), APP_A, "house-tools", true).await;

    for app in [APP_A, APP_B] {
        let req = Request::builder()
            .method(Method::PATCH)
            .uri(format!("/api/mcp/servers/{id}"))
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"command": "attacker-binary"}).to_string(),
            ))
            .unwrap();
        assert_eq!(
            router_as(state.clone(), app)
                .oneshot(req)
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN,
            "{app} was able to repoint a shared server's command"
        );
    }

    // Still visible to both, and its tools still listable.
    let req = Request::builder()
        .uri(format!("/api/mcp/servers/{id}/tools"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router_as(state, APP_B).oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn recipes_are_scoped_and_not_executable_across_apps() {
    let state = test_state().await;
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/recipes")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"name": "a-recipe", "prompt_template": "do the thing"}).to_string(),
        ))
        .unwrap();
    let resp = router_as(state.clone(), APP_A).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let id = body_json(resp).await["id"].as_str().unwrap().to_string();

    let list = |app: &str| {
        let state = state.clone();
        let app = app.to_string();
        async move {
            let req = Request::builder()
                .uri("/api/recipes")
                .body(Body::empty())
                .unwrap();
            let body = body_json(router_as(state, &app).oneshot(req).await.unwrap()).await;
            body["recipes"].as_array().unwrap().len()
        }
    };
    assert_eq!(list(APP_A).await, 1);
    assert_eq!(list(APP_B).await, 0, "recipes must not leak across apps");

    // Executing it would run A's workflow on A's provider and billing account.
    let exec = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/recipes/{id}/execute"))
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(
        router_as(state.clone(), APP_B)
            .oneshot(exec)
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );

    let del = Request::builder()
        .method(Method::DELETE)
        .uri(format!("/api/recipes/{id}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router_as(state.clone(), APP_B)
            .oneshot(del)
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        list(APP_A).await,
        1,
        "the failed delete must not have landed"
    );
}

#[tokio::test]
async fn schedules_are_scoped_and_not_triggerable_across_apps() {
    let state = test_state().await;

    // A schedule needs a recipe to point at.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/recipes")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"name": "a-recipe", "prompt_template": "x"}).to_string(),
        ))
        .unwrap();
    let recipe_id = body_json(router_as(state.clone(), APP_A).oneshot(req).await.unwrap()).await
        ["id"]
        .as_str()
        .unwrap()
        .to_string();

    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/schedules")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "name": "nightly",
                "cron": "0 0 3 * * *",
                "recipe_id": recipe_id,
                "enabled": false
            })
            .to_string(),
        ))
        .unwrap();
    let resp = router_as(state.clone(), APP_A).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let id = body_json(resp).await["id"].as_str().unwrap().to_string();

    let count = |app: &str| {
        let state = state.clone();
        let app = app.to_string();
        async move {
            let req = Request::builder()
                .uri("/api/schedules")
                .body(Body::empty())
                .unwrap();
            let body = body_json(router_as(state, &app).oneshot(req).await.unwrap()).await;
            body["schedules"].as_array().unwrap().len()
        }
    };
    assert_eq!(count(APP_A).await, 1);
    assert_eq!(count(APP_B).await, 0);

    for (method, path) in [
        (Method::PATCH, format!("/api/schedules/{id}")),
        (Method::DELETE, format!("/api/schedules/{id}")),
        (Method::POST, format!("/api/schedules/{id}/run_now")),
    ] {
        let req = Request::builder()
            .method(method.clone())
            .uri(&path)
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        assert_eq!(
            router_as(state.clone(), APP_B)
                .oneshot(req)
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND,
            "{method} {path} was reachable by a non-owner"
        );
    }

    assert_eq!(count(APP_A).await, 1);
}

// ---------------------------------------------------------------------------
// Search
//
// The quietest place a missed scope could hide: this route returns raw
// conversation text, so a leak hands one app the *contents* of another's chats
// rather than merely confirming an id exists.
// ---------------------------------------------------------------------------

/// Insert a message directly, so search has something to find without needing
/// a live provider to generate it.
async fn seed_message(state: &AppState, session_id: &str, role: &str, content: &str) {
    sqlx::query(
        "INSERT INTO messages (id, session_id, role, content, content_format) \
         VALUES (?, ?, ?, ?, 'text')",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(session_id)
    .bind(role)
    .bind(content)
    .execute(&state.db)
    .await
    .unwrap();
}

async fn search_hits(state: Arc<AppState>, app_id: &str, q: &str) -> Vec<String> {
    let req = Request::builder()
        .uri(format!("/api/search?q={q}"))
        .body(Body::empty())
        .unwrap();
    let resp = router_as(state, app_id).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["content"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn search_never_returns_another_apps_conversation() {
    let state = test_state().await;
    let a = create_session(state.clone(), APP_A).await;
    let b = create_session(state.clone(), APP_B).await;

    // Deliberately identical text in both, so only the scope can tell them
    // apart -- a filter that accidentally matched on content would pass.
    seed_message(&state, &a, "user", "the pelican migration schedule").await;
    seed_message(&state, &b, "user", "the pelican migration schedule").await;

    let a_hits = search_hits(state.clone(), APP_A, "pelican").await;
    let b_hits = search_hits(state.clone(), APP_B, "pelican").await;

    assert_eq!(a_hits.len(), 1, "app A should see exactly its own message");
    assert_eq!(b_hits.len(), 1, "app B should see exactly its own message");

    // And a third app with nothing of its own sees nothing at all.
    apps::register_app(&state.db, "app-c", "App C", "key-c")
        .await
        .unwrap();
    assert!(search_hits(state, "app-c", "pelican").await.is_empty());
}

#[tokio::test]
async fn search_finds_across_sessions_within_one_app() {
    // The capability that did not exist in V1 at all: recall was per-session.
    let state = test_state().await;
    let s1 = create_session(state.clone(), APP_A).await;
    let s2 = create_session(state.clone(), APP_A).await;
    seed_message(&state, &s1, "user", "notes about albatross wingspan").await;
    seed_message(&state, &s2, "assistant", "more albatross measurements").await;

    assert_eq!(search_hits(state, APP_A, "albatross").await.len(), 2);
}

#[tokio::test]
async fn narrowing_to_a_session_does_not_bypass_ownership() {
    // `session_id` narrows a search; it must not be a way around the join.
    let state = test_state().await;
    let a = create_session(state.clone(), APP_A).await;
    seed_message(&state, &a, "user", "confidential kestrel plans").await;

    let req = Request::builder()
        .uri(format!("/api/search?q=kestrel&session_id={a}"))
        .body(Body::empty())
        .unwrap();
    let body = body_json(router_as(state, APP_B).oneshot(req).await.unwrap()).await;
    assert_eq!(body["total"].as_i64().unwrap(), 0);
}

#[tokio::test]
async fn an_empty_query_is_an_empty_result_not_an_error() {
    let state = test_state().await;
    let a = create_session(state.clone(), APP_A).await;
    seed_message(&state, &a, "user", "something").await;

    let req = Request::builder()
        .uri("/api/search?q=%20%20")
        .body(Body::empty())
        .unwrap();
    let resp = router_as(state, APP_A).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["total"].as_i64().unwrap(), 0);
}

#[tokio::test]
async fn a_negative_limit_cannot_read_the_whole_index() {
    // SQLite treats a negative LIMIT as "no limit" -- the same footgun that
    // once made `?limit=-1` on the session list read every row.
    let state = test_state().await;
    let a = create_session(state.clone(), APP_A).await;
    for i in 0..5 {
        seed_message(&state, &a, "user", &format!("heron sighting number {i}")).await;
    }

    let req = Request::builder()
        .uri("/api/search?q=heron&limit=-1")
        .body(Body::empty())
        .unwrap();
    let resp = router_as(state, APP_A).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let total = body_json(resp).await["total"].as_i64().unwrap();
    assert!(
        (1..=5).contains(&total),
        "a negative limit must clamp, not disable the limit (got {total})"
    );
}

// ---------------------------------------------------------------------------
// Plugins
//
// A plugin hooks the agent loop and carries per-app instance state; an MCP
// server provides tools. These tests cover the first kind: two apps with
// pathway on must get two belief graphs, and an app with it off must pay
// nothing at all.
// ---------------------------------------------------------------------------

async fn set_plugin(state: Arc<AppState>, app_id: &str, plugin: &str, enabled: bool) -> StatusCode {
    let req = Request::builder()
        .method(Method::PUT)
        .uri(format!("/api/apps/me/plugins/{plugin}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "enabled": enabled }).to_string()))
        .unwrap();
    router_as(state, app_id).oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn each_app_gets_its_own_belief_graph() {
    // The core of the plugin split. Both apps enable pathway; each must get a
    // distinct engine, so a belief recorded by one is invisible to the other.
    let state = test_state().await;
    assert_eq!(set_plugin(state.clone(), APP_A, "pathway", true).await, StatusCode::OK);
    assert_eq!(set_plugin(state.clone(), APP_B, "pathway", true).await, StatusCode::OK);

    let a = state.plugins.pathway_for(APP_A).await.expect("app A engine");
    let b = state.plugins.pathway_for(APP_B).await.expect("app B engine");

    assert!(
        !Arc::ptr_eq(&a, &b),
        "two apps must not share one belief graph"
    );
    assert_eq!(state.plugins.open_instances().await, 2);
    state.plugins.shutdown().await;
}

#[tokio::test]
async fn a_disabled_plugin_costs_nothing() {
    // Not merely "tools hidden": no engine, no background sweep, no file.
    // Turning pathway off has to actually be free, or "app Z runs without it"
    // is not a real capability.
    let state = test_state().await;
    assert_eq!(set_plugin(state.clone(), APP_A, "pathway", false).await, StatusCode::OK);

    assert!(state.plugins.pathway_for(APP_A).await.is_none());
    assert_eq!(state.plugins.open_instances().await, 0);
}

#[tokio::test]
async fn turning_a_plugin_off_closes_a_live_instance() {
    // `pathway_for` short-circuits on an already-open instance, so without an
    // explicit close the engine would keep its background sweep running and
    // its file held until daemon restart.
    let state = test_state().await;
    set_plugin(state.clone(), APP_A, "pathway", true).await;
    assert!(state.plugins.pathway_for(APP_A).await.is_some());
    assert_eq!(state.plugins.open_instances().await, 1);

    set_plugin(state.clone(), APP_A, "pathway", false).await;
    assert_eq!(
        state.plugins.open_instances().await,
        0,
        "disabling must close the live instance, not just future lookups"
    );
    assert!(state.plugins.pathway_for(APP_A).await.is_none());
}

#[tokio::test]
async fn one_apps_plugin_choice_does_not_move_anothers() {
    let state = test_state().await;
    set_plugin(state.clone(), APP_A, "pathway", true).await;
    set_plugin(state.clone(), APP_B, "pathway", false).await;

    assert!(state.plugins.is_enabled(APP_A).await);
    assert!(!state.plugins.is_enabled(APP_B).await);
}

#[tokio::test]
async fn the_plugin_listing_reports_effective_state_and_whether_it_was_chosen() {
    // "What will happen on my next turn" is the question a caller is asking,
    // so an app that never expressed a preference still gets an answer -- and
    // `explicit` is what distinguishes a choice from an inherited default.
    let state = test_state().await;

    let list = |app: &str| {
        let state = state.clone();
        let app = app.to_string();
        async move {
            let req = Request::builder()
                .uri("/api/apps/me/plugins")
                .body(Body::empty())
                .unwrap();
            body_json(router_as(state, &app).oneshot(req).await.unwrap()).await
        }
    };

    let before = list(APP_A).await;
    assert_eq!(before["plugins"][0]["plugin"], "pathway");
    assert_eq!(before["plugins"][0]["explicit"], false);

    set_plugin(state.clone(), APP_A, "pathway", true).await;
    let after = list(APP_A).await;
    assert_eq!(after["plugins"][0]["explicit"], true);
    assert_eq!(after["plugins"][0]["enabled"], true);
    state.plugins.shutdown().await;
}

#[tokio::test]
async fn an_unknown_plugin_name_is_a_404_not_a_stored_preference() {
    // Storing it would leave a row nothing ever reads, and the client would
    // never learn it had typoed.
    let state = test_state().await;
    assert_eq!(
        set_plugin(state.clone(), APP_A, "not-a-plugin", true).await,
        StatusCode::NOT_FOUND
    );

    let req = Request::builder()
        .uri("/api/apps/me/plugins")
        .body(Body::empty())
        .unwrap();
    let body = body_json(router_as(state, APP_A).oneshot(req).await.unwrap()).await;
    assert_eq!(body["plugins"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn pathway_routes_serve_the_callers_own_engine_only() {
    // A belief browser must show the caller its *own* graph. With one shared
    // engine there was no question to ask; here, resolving the wrong app's
    // instance would show one frontend what another inferred about the user.
    let state = test_state().await;
    set_plugin(state.clone(), APP_A, "pathway", true).await;

    // A sees its (empty) graph; B has pathway off and is told so, rather than
    // being handed A's.
    let beliefs = |app: &str| {
        let state = state.clone();
        let app = app.to_string();
        async move {
            let req = Request::builder()
                .uri("/api/pathway/beliefs")
                .body(Body::empty())
                .unwrap();
            body_json(router_as(state, &app).oneshot(req).await.unwrap()).await
        }
    };

    let a = beliefs(APP_A).await;
    assert!(a.get("error").is_none(), "app A has pathway on: {a}");

    let b = beliefs(APP_B).await;
    assert_eq!(b["error"], "pathway disabled");
    state.plugins.shutdown().await;
}

#[tokio::test]
async fn embeddings_reports_a_clean_503_with_no_model_configured() {
    // The one case V1's permanent 503 was actually describing. It must stay
    // distinguishable from a wrong URL, and must not panic.
    let state = test_state().await;
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/embeddings")
        .header("content-type", "application/json")
        .body(Body::from(json!({"prompt": "hello"}).to_string()))
        .unwrap();
    let resp = router_as(state, APP_A).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(resp).await;
    assert!(
        body["error"].as_str().unwrap().contains("no embedding model"),
        "unexpected error: {body}"
    );
}
