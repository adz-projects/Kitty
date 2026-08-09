pub mod chat;
pub mod embeddings;
pub mod health;
pub mod mcp;
pub mod memory;
pub mod pathway;
pub mod providers;
pub mod recipes;
pub mod schedules;

use std::sync::Arc;

use axum::routing::{delete, get, patch, post};
use axum::Router;

use crate::agent::Agent;
use crate::config::BigTinyConfig;
use crate::mcp::MCPManager;
use crate::provider::router::ProviderRouter;
use crate::recipes::engine::RecipeEngine;
use crate::scheduler::Scheduler;

/// Shared state handed to every route handler.
pub struct AppState {
    pub db: sqlx::SqlitePool,
    pub agent: Arc<Agent>,
    pub mcp: Arc<MCPManager>,
    pub router: Arc<ProviderRouter>,
    pub recipe_engine: Arc<RecipeEngine>,
    pub scheduler: Arc<tokio::sync::Mutex<Scheduler>>,
    pub config: BigTinyConfig,
    /// Behavioral-memory engine. `None` when disabled.
    pub pathway: Option<Arc<adaptive_pathway::engine::PathwayEngine>>,
    /// Resident local-model slots (docs/ANDROID.md §4.1). Cheap to construct
    /// and empty until something asks for a model, so it is unconditional
    /// rather than an `Option` — "no local engine configured" is reported by
    /// the slot itself, which keeps the error specific.
    #[cfg(feature = "local-engine")]
    pub local_slots: crate::local::SlotManager,
}

/// Builds the full route table. Paths/methods mirror
/// `plugins/bigtiny/bigtiny/server/routes/*.py` exactly — Kitty's existing
/// Rust client (`src-tauri/src/bigtiny/*.rs`) depends on this wire shape.
/// Auth/error/logging middleware and CORS are layered on separately in
/// `lib.rs::run()` (Phase E/G), not here.
pub fn create_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/health", get(health::check_health))
        .route("/api/status", get(health::status))
        .route("/api/memory/stats", get(memory::stats))
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
        .route("/api/chat/{id}/send", post(chat::send_message))
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
            "/api/recipes",
            get(recipes::list_recipes).post(recipes::create_recipe),
        )
        .route("/api/recipes/{id}", delete(recipes::delete_recipe))
        .route("/api/recipes/{id}/execute", post(recipes::execute_recipe))
        .route(
            "/api/schedules",
            get(schedules::list_schedules).post(schedules::create_schedule),
        )
        .route(
            "/api/schedules/{id}",
            patch(schedules::update_schedule).delete(schedules::delete_schedule),
        )
        .route("/api/schedules/{id}/run_now", post(schedules::run_now))
        .with_state(state)
}
