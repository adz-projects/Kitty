pub mod agent;
pub mod config;
pub mod crypto;
pub mod error;
pub mod hitl;
pub mod mcp;
pub mod models;
pub mod network;
pub mod provider;
pub mod pyrepr;
pub mod recipes;
pub mod routes;
pub mod scheduler;
pub mod server;
pub mod storage;

use std::sync::Arc;

use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::CorsLayer;

use agent::summarizer::SummarizerClient;
use agent::Agent;
use config::BigTinyConfig;
use error::DaemonError;
use hitl::manager::HITLManager;
use mcp::MCPManager;
use provider::router::ProviderRouter;
use recipes::engine::RecipeEngine;
use scheduler::Scheduler;

/// Everything the CLI entry point (or an embedding host, e.g. Kitty's Rust
/// core linking this crate directly instead of spawning a subprocess) needs
/// to hand `run()` beyond the config file itself.
#[derive(Default)]
pub struct RunOptions {
    pub host: String,
    /// Port to bind. `0` asks the OS for an ephemeral port — pair this with
    /// `ready_tx` to learn which one was actually chosen, since nothing else
    /// reports it back.
    pub port: u16,
    pub db_path: String,
    pub secret: Option<String>,
    /// When `true`, a `None` secret is treated as a misconfiguration and
    /// every `/api/*` route except `/api/health` is denied, rather than
    /// running unauthenticated. Loopback is not process-private on every
    /// platform (notably Android, where any app holding `INTERNET` can reach
    /// `127.0.0.1`), so an embedding host on such a platform should set this
    /// `true` and always supply a `secret`. Desktop's CLI entry point leaves
    /// this `false` to preserve existing single-user-localhost behavior. See
    /// `server::middleware::AuthConfig`.
    pub require_secret: bool,
    pub recipes_dir: std::path::PathBuf,
    /// BigTiny's app-data directory (respects `BIGTINY_DATA_DIR`) — also
    /// used as the sandbox's always-allowed "cache dir"
    /// (`agent::sandbox::CACHE_DIR`'s real, non-fallback value).
    pub data_dir: String,
    /// Stable, hex-encoded 32-byte at-rest encryption key for provider API
    /// keys / MCP server auth headers — Kitty generates and persists this
    /// once in Windows Credential Manager and passes it via env on every
    /// launch (unlike `secret` above, which regenerates every launch and so
    /// can't double as this). `None` for a standalone run with no Kitty
    /// parent process — `crypto::init` falls back to a self-managed key
    /// file in `data_dir` in that case.
    pub encryption_key: Option<String>,
    /// Signalled once the listener is bound, with the actual address (useful
    /// with `port: 0`). An embedding host awaits this instead of pre-picking
    /// a free port and racing `run()` to bind it first.
    pub ready_tx: Option<tokio::sync::oneshot::Sender<std::net::SocketAddr>>,
    /// Lets an embedding host stop the daemon without a process signal.
    /// `run()` still also honors ctrl-c/SIGTERM (matching CLI usage) —
    /// whichever fires first triggers the same graceful shutdown sequence.
    /// The CLI entry point leaves this `None`.
    pub shutdown: Option<tokio::sync::oneshot::Receiver<()>>,
}

/// Construct every subsystem and serve, mirroring
/// `plugins/bigtiny/bigtiny/server/app.py`'s `lifespan()` startup/shutdown
/// order exactly. Runs until a ctrl-c/SIGTERM, then tears down in the same
/// order Python does (scheduler -> agent -> mcp), db pool dropped last via
/// `SqlitePool`'s own `Drop`.
pub async fn run(config: BigTinyConfig, options: RunOptions) -> Result<(), DaemonError> {
    // Must run before anything below that might decrypt a stored value
    // (`router.load_providers`, `mcp.connect_all`).
    crypto::init(
        std::path::Path::new(&options.data_dir),
        options.encryption_key.as_deref(),
    )?;

    let db = storage::Database::connect(&options.db_path).await?;
    let pool = db.pool().clone();

    let mcp = Arc::new(MCPManager::new(pool.clone()));
    mcp.connect_all().await; // isolated per-server failure, matches Python's connect_all

    let router = Arc::new(ProviderRouter::new(config.cache.clone()));
    router.load_providers(&pool).await?;

    let hitl = Arc::new(tokio::sync::Mutex::new(HITLManager::new(
        pool.clone(),
        config.hitl.clone(),
    )));
    let summarizer = Arc::new(SummarizerClient::new(config.summarizer.clone()));
    let agent = Arc::new(Agent::new(
        pool.clone(),
        router.clone(),
        mcp.clone(),
        hitl,
        summarizer,
        config.clone(),
        options.data_dir.clone(),
    ));

    let recipe_engine = Arc::new(RecipeEngine::new(
        pool.clone(),
        agent.clone(),
        mcp.clone(),
        options.recipes_dir.clone(),
    ));

    let mut scheduler = Scheduler::new(pool.clone(), recipe_engine.clone()).await?;
    if config.scheduler.enabled {
        if let Err(e) = scheduler.start().await {
            tracing::warn!("Scheduler failed to start: {e}");
        }
    }
    let scheduler = Arc::new(tokio::sync::Mutex::new(scheduler));

    let state = Arc::new(routes::AppState {
        db: pool,
        agent: agent.clone(),
        mcp: mcp.clone(),
        router,
        recipe_engine,
        scheduler: scheduler.clone(),
        config: config.clone(),
    });

    let auth = Arc::new(server::middleware::AuthConfig {
        secret: options.secret.clone(),
        required: options.require_secret,
    });
    let app = routes::create_router(state)
        .layer(axum::middleware::from_fn(
            server::middleware::request_logging_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            auth,
            server::middleware::auth_middleware,
        ))
        .layer(CatchPanicLayer::new())
        // No cross-origin caller has a legitimate reason to hit this API:
        // Kitty's own webview never fetches localhost directly (all I/O goes
        // through the Rust host, by design — see CLAUDE.md), and an
        // embedding host talks to it over plain HTTP, which CORS (a
        // browser-enforced policy, not a server-side request filter) never
        // touches. `CorsLayer::new()` allows no origins, closing off the one
        // consumer class this could ever matter for: a page loaded in some
        // *other* browser tab trying to reach this port.
        .layer(CorsLayer::new());

    let listener = tokio::net::TcpListener::bind((options.host.as_str(), options.port)).await?;
    let bound_addr = listener.local_addr()?;
    tracing::info!("BigTiny listening on {bound_addr}");
    if let Some(ready_tx) = options.ready_tx {
        let _ = ready_tx.send(bound_addr);
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(options.shutdown))
        .await?;

    scheduler.lock().await.stop().await;
    agent.shutdown().await;
    mcp.disconnect_all().await;

    Ok(())
}

/// Resolves on whichever comes first: a ctrl-c/SIGTERM (CLI usage) or the
/// embedding host closing `RunOptions::shutdown` (in-process usage, e.g.
/// Kitty's Rust core stopping the daemon without a process signal to send).
async fn shutdown_signal(shutdown: Option<tokio::sync::oneshot::Receiver<()>>) {
    match shutdown {
        Some(rx) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = rx => {}
            }
        }
        None => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
    tracing::info!("Shutdown signal received");
}
