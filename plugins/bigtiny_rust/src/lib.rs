pub mod agent;
pub mod config;
pub mod crypto;
pub mod env_contract;
pub mod error;
pub mod hitl;
/// In-process llama.cpp engine (docs/ANDROID.md §3). Feature-gated: see
/// `Cargo.toml`'s `local-engine`.
#[cfg(feature = "local-engine")]
pub mod local;
pub mod mcp;
pub mod models;
pub mod network;
pub mod provider;
pub mod recipes;
pub mod routes;
pub mod scheduler;
pub mod server;
pub mod storage;

use std::sync::Arc;

/// Provider id the in-process engine registers under.
///
/// Public because it's part of the wire contract, not an internal detail: a
/// client pins a session to the local engine by sending this exact string as
/// `provider` to `POST /api/chat/`. `tools/local_engine_lab.py` hard-codes the
/// same value.
#[cfg(feature = "local-engine")]
pub const LOCAL_PROVIDER_ID: &str = "local";

use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::CorsLayer;

use agent::summarizer_chain::SummarizerChain;
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

    // Constructed before both the pathway engine and the router so all three
    // share ONE set of resident models — otherwise chat, `/api/embeddings` and
    // adaptive-pathway would each load their own copy of the same GGUF.
    #[cfg(feature = "local-engine")]
    let local_slots = local::SlotManager::new();

    // Behavioral-memory engine. `None` when disabled — the in-process
    // `"pathway"` MCP server is then simply not constructed (configured off,
    // not "race lost").
    let pathway_engine: Option<Arc<adaptive_pathway::engine::PathwayEngine>> = if config
        .pathway
        .enabled
    {
        let db_path = std::path::Path::new(&options.data_dir).join(&config.pathway.db_name);

        // Embeddings run in-process when the local engine can serve them —
        // no HTTP hop to our own listener. `space_tag` must move in lockstep
        // with the weights (see its doc comment): it is what tells
        // `reembed_stale_beliefs` that beliefs embedded by a previous model
        // need migrating.
        #[cfg(feature = "local-engine")]
        let (embedder, ap_config) = {
            let mut ap_config = adaptive_pathway::config::Config::default();
            let e = local::pathway_embed::embedder_for(&local_slots, &config.local);
            if e.is_some() {
                ap_config.embedding.ollama_model =
                    local::LocalPathwayEmbedder::space_tag(&config.local.embed_model_path);
                tracing::info!(
                    space = %ap_config.embedding.ollama_model,
                    "adaptive-pathway embeddings served in-process by the local engine"
                );
            }
            (e, ap_config)
        };
        // Without the engine compiled in, AP keeps its own behaviour: HTTP to
        // a configured Ollama, else lexical hashing.
        #[cfg(not(feature = "local-engine"))]
        let (embedder, ap_config): (
            Option<Arc<dyn adaptive_pathway::embed::SemanticEmbedder>>,
            _,
        ) = (None, adaptive_pathway::config::Config::default());

        match adaptive_pathway::engine::PathwayEngine::open_with_embedder(
            &db_path.to_string_lossy(),
            ap_config,
            embedder,
        )
        .await
        {
            Ok(e) => Some(e),
            Err(err) => {
                tracing::warn!(
                    "pathway engine failed to open at {}: {err}",
                    db_path.display()
                );
                None
            }
        }
    } else {
        None
    };

    let mcp = Arc::new(MCPManager::new(pool.clone(), pathway_engine.clone()));
    mcp.connect_all().await; // isolated per-server failure, matches Python's connect_all

    let router = Arc::new(ProviderRouter::new(config.cache.clone()));
    router.load_providers(&pool).await?;

    // In-process llama.cpp engine (docs/ANDROID.md §3, D1). Registered under
    // the fixed id `"local"`, which a session pins at birth via
    // `POST /api/chat/`'s `provider` field — no DB row is involved, which is
    // just as well since `provider_type` is CHECK-constrained to
    // `('openai_compat','anthropic')`.
    #[cfg(feature = "local-engine")]
    if config.local.enabled {
        router.register_local(
            LOCAL_PROVIDER_ID,
            crate::config::ProviderConfig {
                // Budget against the engine's context size, not the
                // daemon-wide `token_management.max_context_tokens` (64k)
                // that a `None` here falls back to — the engine's real
                // context defaults to 4096, so compaction never fired before
                // the context hard-failed. The resolved (fitted/estimated)
                // value only exists once a model is resident; this is the
                // static resolution the engine itself bottoms out at.
                context_length: Some(
                    local::engine::registration_n_ctx(&config.local) as i32,
                ),
                ..Default::default()
            },
            config.local.clone(),
            local_slots.clone(),
        );
        tracing::info!(
            provider_id = LOCAL_PROVIDER_ID,
            model = %config.local.model_path,
            embed_model = %config.local.embed_model_path,
            "local engine registered"
        );
    } else {
        // Say so explicitly. A silently-absent provider is indistinguishable
        // from a broken one at the point the request fails.
        tracing::info!("local engine not registered ([local].enabled = false)");
    }

    let hitl = Arc::new(tokio::sync::Mutex::new(HITLManager::new(
        pool.clone(),
        config.hitl.clone(),
    )));

    // §4.3's chain, first leg: the in-process engine, when one is configured.
    // Shares `local_slots` with chat and `/api/embeddings` — a summarization
    // pass reuses the already-resident chat model rather than loading a
    // second copy, when the two happen to be the same GGUF (§4.1).
    #[cfg(feature = "local-engine")]
    let local_summarizer = config.local.enabled.then(|| {
        Arc::new(local::LocalSummarizer::new(
            config.local.clone(),
            config.summarizer.clone(),
            local_slots.clone(),
        ))
    });
    #[cfg(feature = "local-engine")]
    let summarizer = Arc::new(SummarizerChain::new(
        local_summarizer,
        router.clone(),
        config.summarizer.clone(),
    ));
    #[cfg(not(feature = "local-engine"))]
    let summarizer = Arc::new(SummarizerChain::new(router.clone(), config.summarizer.clone()));

    // Pathway background learning loop: idle sweep + maintenance, aborted
    // before agent shutdown. Only spawned when the engine is available.
    let (pathway_shutdown_tx, pathway_shutdown_rx) = tokio::sync::watch::channel(false);
    let pathway_shutdown = pathway_engine.as_ref().map(|engine| {
        let engine = engine.clone();
        let host_pool = pool.clone();
        let chat = summarizer.clone();
        tokio::spawn(async move {
            adaptive_pathway::background::run(engine, host_pool, chat, pathway_shutdown_rx).await;
        });
        pathway_shutdown_tx
    });

    let agent = Arc::new(Agent::new(
        pool.clone(),
        router.clone(),
        mcp.clone(),
        hitl,
        summarizer,
        config.clone(),
        options.data_dir.clone(),
        pathway_engine.clone(),
        pathway_shutdown,
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
        pathway: pathway_engine.clone(),
        // Shared with the registered `LocalProvider` above (not a fresh one),
        // so a model loaded for chat is reused for embeddings and vice versa.
        // Starts empty; models load lazily on first use so daemon startup
        // never blocks on a multi-hundred-MB read.
        #[cfg(feature = "local-engine")]
        local_slots,
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

    // Cap on the post-signal HTTP drain: graceful shutdown waits for
    // in-flight connections, and a hung agent turn must not block SIGTERM
    // indefinitely (until now the drain completed BEFORE `agent.shutdown()`
    // aborted turns — a hung turn held the drain open forever, and the
    // supervisor's kill -9 was the only way out).
    const SHUTDOWN_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    let (signal_tx, signal_rx) = tokio::sync::oneshot::channel::<()>();
    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        shutdown_signal(options.shutdown).await;
        let _ = signal_tx.send(());
    });
    // `WithGracefulShutdown` is only `IntoFuture` (and the trait isn't in
    // the 2021 prelude), so wrap it to get a future we can pin and select
    // on.
    let server = async move { serve.await };
    tokio::pin!(server);

    tokio::select! {
        res = &mut server => res?,
        _ = signal_rx => {
            // Signal received; the HTTP drain is now in progress. Tear the
            // subsystems down CONCURRENTLY with it rather than after it —
            // aborting in-flight turns is also what lets their SSE
            // connections close, so this shortens the drain instead of
            // competing with it.
            scheduler.lock().await.stop().await;
            agent.shutdown().await;
            mcp.disconnect_all().await;
            match tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, &mut server).await {
                // The drain finished inside the cap — propagate a serve
                // error the way the original `await?` did.
                Ok(res) => res?,
                Err(_) => {
                    tracing::warn!(
                        "graceful HTTP drain exceeded {SHUTDOWN_DRAIN_TIMEOUT:?}; forcing shutdown"
                    );
                }
            }
            return Ok(());
        }
    }

    // The server drained on its own (no in-flight connections when the
    // signal landed) — the subsystems still need their teardown.
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
