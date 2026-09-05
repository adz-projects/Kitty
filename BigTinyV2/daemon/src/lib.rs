pub mod agent;
pub mod config;
pub mod discovery;
pub mod crypto;
pub mod env_contract;
pub mod error;
pub mod hitl;
#[cfg(feature = "litert-embed")]
pub mod litert;
pub mod mcp;
pub mod models;
pub mod network;
pub mod plugins;
pub mod provider;
pub mod recipes;
pub mod routes;
pub mod scheduler;
pub mod server;
pub mod storage;

use std::sync::Arc;

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
    /// Exit after this many minutes with no authenticated request, no
    /// in-flight turn, and no running job. `None` disables it.
    ///
    /// This is what replaces V1's "the app that spawned me kills me on
    /// exit" lifetime. With several clients, no single one of them may
    /// decide the daemon is finished -- so the daemon decides for itself.
    pub idle_exit_mins: Option<u64>,
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

    // Anything still `running` belongs to a previous process that stopped
    // without finishing it. Marked `interrupted`, never re-queued: a turn may
    // have executed tools with side effects, and silently re-running it would
    // repeat them. The owner decides whether resubmitting is safe.
    match storage::jobs::mark_interrupted_on_boot(&pool).await {
        Ok(0) => {}
        Ok(n) => tracing::warn!("marked {n} job(s) interrupted by a previous shutdown"),
        Err(e) => tracing::warn!("could not sweep interrupted jobs: {e}"),
    }

    // Behavioral-memory plugin. V1 opened exactly ONE engine here, for the
    // whole daemon. V2 opens one per app, lazily, through `PluginHost` — a
    // single graph shared by several frontends would mix their beliefs, which
    // is both a privacy leak and a quality regression (beliefs blended across
    // two unrelated usage patterns describe nobody in particular).
    //
    // What stays shared is everything expensive: the embedder below is one
    // loaded model behind an `Arc`, and the config that tags its vector space.
    let (ap_config, ap_embedder) = {
        // Embeddings run in-process (no HTTP hop to our own listener) via the
        // LiteRT engine (EmbeddingGemma); when it isn't configured, AP keeps its
        // own behaviour (HTTP Ollama / lexical hashing). The space tag
        // (`ap_config.embedding.ollama_model`) must move in lockstep with the
        // weights so `reembed_stale_beliefs` migrates beliefs on a model change.
        #[allow(unused_mut)]
        let mut ap_config = adaptive_pathway::config::Config::default();
        #[allow(unused_mut)]
        let mut embedder: Option<Arc<dyn adaptive_pathway::embed::SemanticEmbedder>> = None;

        #[cfg(feature = "litert-embed")]
        if config.litert.enabled && !config.litert.embed_model_path.trim().is_empty() {
            let stem = std::path::Path::new(&config.litert.embed_model_path)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".into());
            ap_config.embedding.ollama_model = format!("litert:{stem}");
            embedder = Some(Arc::new(litert::LiteRtEmbedder::spawn(
                config.litert.lib_path.clone(),
                config.litert.embed_model_path.clone(),
                config.litert.tokenizer_path.clone(),
            )));
            tracing::info!(
                space = %ap_config.embedding.ollama_model,
                "adaptive-pathway embeddings served in-process by LiteRT"
            );
        }

        (ap_config, embedder)
    };

    // No daemon-wide engine to hand over: the in-process `pathway` MCP server
    // is now connected per app, holding that app's engine.
    let mcp = Arc::new(MCPManager::with_data_dir(
        pool.clone(),
        None,
        std::path::PathBuf::from(&options.data_dir),
    ));
    mcp.connect_all().await; // isolated per-server failure, matches Python's connect_all
    // Supervisor: retires the tools of a server whose transport died and
    // brings enabled-but-down servers back with exponential backoff. Without
    // it `connect_all` above is the only connect attempt for the whole
    // process lifetime.
    let mcp_health_watcher = mcp.clone().spawn_health_watcher();

    let router = Arc::new(ProviderRouter::new(config.cache.clone()));
    router.load_providers(&pool).await?;

    // No in-process *chat* engine: local chat is not a product use case (the
    // retired llama.cpp `local` provider is gone). Chat always routes to a
    // configured remote provider; the only local roles are LiteRT embeddings
    // (above) and, on Windows, LiteRT-LM compaction summarization (below).

    let hitl = Arc::new(tokio::sync::Mutex::new(HITLManager::new(
        pool.clone(),
        config.hitl.clone(),
    )));

    // §4.3's chain, first leg: the in-process summarizer, when one is
    // configured. LiteRT-LM (Windows-only generative) is the only local leg;
    // `None` = the chain goes straight to the session/router model (Android's
    // path — no generative model on the phone).
    #[allow(unused_mut)]
    let mut local_summarizer: Option<
        Arc<dyn adaptive_pathway::traits::StructuredChat + Send + Sync>,
    > = None;

    #[cfg(all(windows, feature = "litert-engine"))]
    if config.litert.enabled && !config.litert.summarizer_model_path.trim().is_empty() {
        let s = litert::LiteRtSummarizer::spawn(config.litert.summarizer_model_path.clone());
        if s.is_available() {
            local_summarizer = Some(Arc::new(s));
        }
    }

    let summarizer = Arc::new(SummarizerChain::new(
        local_summarizer,
        router.clone(),
        config.summarizer.clone(),
    ));

    // One background sweep per *live* engine, owned by the host. V1 spawned a
    // single loop here because there was a single engine; carrying that
    // forward naively would mean one idle-sweep loop per registered app,
    // forever, whether or not the app ever sends a turn.
    let plugins = Arc::new(plugins::PluginHost::new(
        pool.clone(),
        std::path::PathBuf::from(&options.data_dir),
        config.pathway.enabled,
        ap_config,
        ap_embedder,
        summarizer.clone(),
    ));

    let agent = Arc::new(Agent::new(
        pool.clone(),
        router.clone(),
        mcp.clone(),
        hitl,
        summarizer,
        config.clone(),
        options.data_dir.clone(),
        plugins.clone(),
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

    // Shared by the auth middleware (which populates it) and the app
    // routes (which invalidate it on revocation), so a deleted app stops
    // authenticating immediately rather than at some TTL boundary.
    // Kept back before the pool is moved into `AppState`, for the idle
    // timer's detached-job check.
    let idle_pool = pool.clone();

    let key_cache = Arc::new(server::middleware::KeyCache::new());

    // Regenerated every launch. This is what lets a client tell "the
    // daemon this handshake describes" from "a different daemon handed the
    // same port after a restart" -- a live PID on the recorded port is not
    // by itself proof. Surfaced on `/api/health` so the check needs no
    // authentication.
    let instance_id = discovery::generate_token();

    let state = Arc::new(routes::AppState {
        db: pool,
        agent: agent.clone(),
        mcp: mcp.clone(),
        router,
        recipe_engine,
        scheduler: scheduler.clone(),
        config: config.clone(),
        plugins: plugins.clone(),
        replay: Arc::new(server::replay::ReplayBuffers::new()),
        key_cache: key_cache.clone(),
        instance_id: instance_id.clone(),
    });

    // Per-app identity replaces V1's single shared secret. `registration_token`
    // is the bootstrap credential published in the handshake file; it
    // authorizes `POST /api/apps/register` and nothing else, and is
    // regenerated every launch so it cannot outlive the daemon that issued it.
    let registration_token = options
        .secret
        .clone()
        .unwrap_or_else(crate::discovery::generate_token);
    let activity = Arc::new(server::middleware::ActivityClock::new());
    let auth = Arc::new(server::middleware::AuthState {
        pool: state.db.clone(),
        cache: key_cache.clone(),
        registration_token: registration_token.clone(),
        activity: activity.clone(),
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

    // Published only now: the listener is bound and migrations have run, so a
    // client that reads and validates this file can assume the daemon will
    // serve its next request. Publishing earlier would turn a startup race
    // into a confusing attach-then-fail.
    let handshake = discovery::build(
        instance_id.clone(),
        &options.host,
        bound_addr.port(),
        std::path::Path::new(&options.data_dir),
        registration_token,
    );
    if let Err(e) = discovery::publish(std::path::Path::new(&options.data_dir), &handshake).await {
        // Not fatal: the daemon still serves anyone holding a key. Only
        // discovery-by-file is lost, and saying so loudly beats refusing to
        // start.
        tracing::error!("failed to publish handshake: {e}");
    }

    // Idle exit. Three conditions, all required -- an idle *connection* is not
    // an idle daemon:
    //   * no authenticated request within the window;
    //   * no turn in flight (a long generation makes no requests meanwhile);
    //   * no job or scheduled run active (detached work has no client at all,
    //     which is precisely why it must not be mistaken for inactivity).
    let idle_shutdown = options.idle_exit_mins.map(|mins| {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let activity = activity.clone();
        let agent = agent.clone();
        let scheduler_for_idle = scheduler.clone();
        let idle_pool = idle_pool.clone();
        tokio::spawn(async move {
            let window = std::time::Duration::from_secs(mins * 60);
            // Check several times per window so the actual exit lands close to
            // the deadline rather than up to a full window late.
            let tick = (window / 4).max(std::time::Duration::from_secs(30));
            let mut ticker = tokio::time::interval(tick);
            ticker.tick().await; // the first tick completes immediately
            loop {
                ticker.tick().await;
                if activity.idle_secs() < window.as_secs() {
                    continue;
                }
                if agent.has_active_turns() {
                    continue;
                }
                if scheduler_for_idle.lock().await.has_running_jobs() {
                    continue;
                }
                // Detached jobs have no client by definition, so the activity
                // clock cannot see them -- and they are exactly the work it
                // would be worst to kill halfway.
                match crate::storage::jobs::list_by_status(&idle_pool, "running", 1).await {
                    Ok(rows) if !rows.is_empty() => continue,
                    Err(e) => {
                        tracing::warn!("idle check could not read jobs: {e}; staying up");
                        continue;
                    }
                    _ => {}
                }
                tracing::info!("idle for {mins} minutes with no active work; shutting down");
                let _ = tx.send(());
                return;
            }
        });
        rx
    });

    // Cap on the post-signal HTTP drain: graceful shutdown waits for
    // in-flight connections, and a hung agent turn must not block SIGTERM
    // indefinitely (until now the drain completed BEFORE `agent.shutdown()`
    // aborted turns — a hung turn held the drain open forever, and the
    // supervisor's kill -9 was the only way out).
    const SHUTDOWN_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    let (signal_tx, signal_rx) = tokio::sync::oneshot::channel::<()>();
    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        // Whichever fires first: ctrl-c/SIGTERM, an embedding host's explicit
        // stop, or the idle timer. All three converge on the same graceful
        // teardown below.
        match idle_shutdown {
            Some(idle) => {
                tokio::select! {
                    _ = shutdown_signal(options.shutdown) => {}
                    _ = idle => {}
                }
            }
            None => shutdown_signal(options.shutdown).await,
        }
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
            mcp_health_watcher.abort();
            mcp.disconnect_all().await;
            plugins.shutdown().await;
            // Withdraw before the drain completes: a client that reads the
            // handshake from here on would be attaching to a daemon already on
            // its way out. Removal is best-effort -- a killed daemon leaves the
            // file behind regardless, which is why clients validate it rather
            // than trusting that it exists.
            discovery::withdraw(std::path::Path::new(&options.data_dir)).await;
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
    mcp_health_watcher.abort();
    mcp.disconnect_all().await;
    plugins.shutdown().await;
    discovery::withdraw(std::path::Path::new(&options.data_dir)).await;

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
