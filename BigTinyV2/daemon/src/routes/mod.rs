pub mod apps;
pub mod chat;
pub mod embeddings;
pub mod health;
pub mod jobs;
pub mod local;
pub mod mcp;
pub mod memory;
pub mod pathway;
pub mod plugins;
pub mod providers;
pub mod schedules;
pub mod search;
pub mod specialists;

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, patch, post};
use axum::Router;

use crate::agent::Agent;
use crate::config::BigTinyConfig;
use crate::mcp::MCPManager;
use crate::provider::router::ProviderRouter;
use crate::scheduler::Scheduler;

/// Request-body ceiling for every route. axum's default is 2 MiB, which a
/// chat send carrying base64 screenshots blows straight past
/// (`Json<SendMessageRequest>` → 413): a full-res PNG is 5–15 MB and
/// base64 inflates by ~4/3, and a send can carry several. 64 MiB leaves
/// generous headroom without making the limit meaningless.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Shared state handed to every route handler.
pub struct AppState {
    pub db: sqlx::SqlitePool,
    pub agent: Arc<Agent>,
    pub mcp: Arc<MCPManager>,
    pub router: Arc<ProviderRouter>,
    /// Runs delegated turns for `/api/specialists/{name}/run` and, through the
    /// `specialists` MCP server, for the model itself. One orchestrator for
    /// both, so the concurrency cap and depth limit cannot be bypassed by
    /// picking the other entry point.
    pub orchestrator: Arc<crate::agent::orchestrator::Orchestrator>,
    pub scheduler: Arc<tokio::sync::Mutex<Scheduler>>,
    pub config: BigTinyConfig,
    /// Loop-integrated plugins, hosted per app. Replaces V1's single
    /// daemon-wide `PathwayEngine`; ask it for the calling app's instance
    /// rather than assuming one exists.
    pub plugins: Arc<crate::plugins::PluginHost>,
    /// Resolved-identity cache, shared with the auth middleware. Held here so
    /// revoking an app can invalidate it synchronously -- a revoked key that
    /// keeps working until a TTL expires is not an acceptable window.
    pub key_cache: Arc<crate::server::middleware::KeyCache>,
    /// Per-turn event buffers, so a client that drops can rejoin a stream
    /// already in progress. Necessary once work outlives its submitter --
    /// see `server::replay`.
    pub replay: crate::server::replay::SharedReplay,
    /// This launch's identity, echoed on `/api/health` so a client can prove
    /// the process answering on a port is the one its handshake describes.
    pub instance_id: String,
}

/// Builds the full route table. Paths/methods mirror
/// `plugins/bigtiny/bigtiny/server/routes/*.py` exactly — Kitty's existing
/// Rust client (`src-tauri/src/bigtiny/*.rs`) depends on this wire shape.
/// Auth/error/logging middleware and CORS are layered on separately in
/// `lib.rs::run()` (Phase E/G), not here.
pub fn create_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/health", get(health::check_health))
        // Registration is gated on the handshake's bootstrap token rather
        // than an app key -- see `server::middleware::auth_middleware`.
        .route("/api/apps/register", post(apps::register))
        .route("/api/apps", get(apps::list))
        .route(
            "/api/apps/me",
            get(apps::get_me).patch(apps::update_me),
        )
        .route("/api/apps/{id}", delete(apps::delete))
        // Plugin selection is its own route family, not folded into
        // `/api/mcp/servers`: a plugin hooks the agent loop and carries
        // per-app instance state, an MCP server provides tools. See
        // `crate::plugins`.
        .route("/api/apps/me/plugins", get(plugins::list))
        .route(
            "/api/apps/me/plugins/{plugin}",
            axum::routing::put(plugins::set).delete(plugins::clear),
        )
        .route("/api/status", get(health::status))
        .route("/api/local/models/status", get(local::models_status))
        .route("/api/memory/stats", get(memory::stats))
        // Full-text search across the calling app's own history. The
        // FTS5 index has existed since migration 009 but was only ever
        // reachable from the internal per-session recall path.
        .route("/api/search", get(search::search))
        // Detached work: submit, poll, collect. A job is a turn with the
        // stream taken away, so it survives its submitter going away.
        .route("/api/jobs", get(jobs::list).post(jobs::create))
        .route("/api/jobs/{id}", get(jobs::get).delete(jobs::cancel))
        // Ollama-compatible on purpose — see routes/embeddings.rs.
        .route("/api/embeddings", post(embeddings::embed))
        .route("/api/pathway/beliefs", get(pathway::list_beliefs))
        .route("/api/pathway/beliefs/{id}", delete(pathway::delete_belief))
        .route("/api/pathway/stats", get(pathway::stats))
        .route(
            "/api/pathway/sessions/{id}/pause",
            patch(pathway::set_paused),
        )
        .route(
            "/api/chat/",
            get(chat::list_sessions).post(chat::create_session),
        )
        .route(
            "/api/chat/{id}",
            patch(chat::rename_session).delete(chat::delete_session),
        )
        .route("/api/chat/{id}/config", patch(chat::update_config))
        .route("/api/chat/{id}/allowed_dirs", get(chat::allowed_dirs))
        .route(
            "/api/chat/{id}/allowed_dirs/revoke",
            post(chat::revoke_dir),
        )
        .route("/api/chat/{id}/send", post(chat::send_message))
        // Rejoin a turn already in progress: a reconnecting client, or a
        // second window. A second *send* still 409s -- that rule is unchanged.
        .route("/api/chat/{id}/stream", get(chat::attach_stream))
        .route("/api/chat/{id}/history", get(chat::get_history))
        .route("/api/chat/{id}/stats", get(chat::get_stats))
        .route("/api/chat/{id}/timings", get(chat::get_timings))
        .route("/api/chat/{id}/pending", get(chat::get_pending))
        .route("/api/chat/{id}/fork", post(chat::fork_session))
        .route("/api/chat/{id}/compact", post(chat::compact_session))
        .route("/api/chat/{id}/cancel", post(chat::cancel_session))
        .route("/api/chat/{id}/approve", post(chat::approve_action))
        .route(
            "/api/providers",
            get(providers::list_providers).post(providers::create_provider),
        )
        .route(
            "/api/providers/{id}",
            patch(providers::update_provider).delete(providers::delete_provider),
        )
        .route("/api/providers/{id}/test", post(providers::test_provider))
        .route("/api/providers/{id}/models", get(providers::list_models))
        .route(
            "/api/mcp/servers",
            get(mcp::list_servers).post(mcp::create_server),
        )
        .route(
            "/api/mcp/servers/{id}",
            patch(mcp::update_server).delete(mcp::delete_server),
        )
        .route("/api/mcp/servers/{id}/connect", post(mcp::connect_server))
        .route("/api/mcp/servers/{id}/tools", get(mcp::list_tools))
        .route(
            "/api/specialists",
            get(specialists::list).post(specialists::create),
        )
        .route("/api/specialists/runs", get(specialists::runs))
        .route("/api/specialists/{id}", delete(specialists::delete))
        .route("/api/specialists/{name}/run", post(specialists::run))
        .route(
            "/api/schedules",
            get(schedules::list_schedules).post(schedules::create_schedule),
        )
        .route(
            "/api/schedules/{id}",
            patch(schedules::update_schedule).delete(schedules::delete_schedule),
        )
        .route("/api/schedules/{id}/run_now", post(schedules::run_now))
        // Applied here rather than in `lib.rs::run()`'s middleware stack so
        // every consumer of `create_router` (including the route smoke
        // tests) gets the same ceiling.
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}
