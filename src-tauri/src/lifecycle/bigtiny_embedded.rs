//! The BigTiny daemon, hosted **inside** the app process (docs/ANDROID.md
//! D8, §2.1/§2.3). Android only.
//!
//! Android 10+ refuses to `exec()` anything in an app-writable directory, so
//! there is no `bigtiny-daemon` executable to spawn here and `externalBin` is
//! empty in `tauri.android.conf.json`. The daemon is linked in instead and
//! started as a library call.
//!
//! **The HTTP boundary is kept.** `bigtiny_rust::run` still binds a loopback
//! listener and Kitty still talks to it through `bigtiny::client`, exactly as
//! on desktop. Calling the daemon's internals directly would have been
//! faster in the narrow sense and would have forked every call site in
//! `bigtiny/` into two implementations — the streaming one especially. One
//! wire protocol for both platforms is worth a loopback hop.
//!
//! **That loopback is not private, which is why D25 exists.** On Android any
//! app holding `INTERNET` can reach `127.0.0.1`, so this host sets
//! `require_secret: true` and always supplies a secret; the daemon then
//! refuses every `/api/*` route but `/api/health` without it. On desktop the
//! same daemon leaves that `false`, because there the listener really is
//! single-user-localhost.

use crate::state::{DaemonHandle, ManagedProcess};

/// Start the daemon on a background task and return once it is listening.
///
/// Mirrors `bigtiny_proc::spawn`'s contract — same `DaemonHandle`, same
/// health semantics — so everything downstream (`bigtiny::client`,
/// `sync_mcp_once_healthy`, the health loop) is identical on both platforms
/// and none of it needs to know which host it got.
///
/// Takes the same settings that function does, minus the three that only mean
/// something for a child process (`command`/`args`/`dir`), so `start_stack`
/// reads config once and both hosts are configured from identical values.
#[allow(clippy::too_many_arguments)]
pub async fn start(
    summarizer: &crate::config::SummarizerSettings,
    token_management: &crate::config::TokenManagementSettings,
    memory: &crate::config::MemorySettings,
    local: &crate::config::LocalModelSettings,
    pathway_enabled: bool,
    pathway_embedding_model: &str,
) -> Result<DaemonHandle, String> {
    let secret = crate::lifecycle::bigtiny_proc::generate_secret();
    let encryption_key = tokio::task::spawn_blocking(
        crate::config::providers::get_or_create_bigtiny_encryption_key,
    )
    .await
    .map_err(|e| format!("encryption key task panicked: {e}"))??;

    // Same variables the desktop host passes as child-process env. Here they
    // go on *our own* process, which is also how the in-process MCP servers
    // (`mcp::builtin`) receive their configuration: `connect` takes no env
    // map, so a linked server reads the host process environment.
    //
    // Safe to set at this point specifically because it happens during
    // startup, before the daemon task exists and before any MCP server is
    // connected — `set_var` is not thread-safe against a concurrent reader.
    for (key, value) in crate::lifecycle::bigtiny_env::daemon_env(
        &secret,
        &encryption_key,
        summarizer,
        token_management,
        memory,
        local,
        pathway_enabled,
        pathway_embedding_model,
    ) {
        std::env::set_var(key, value);
    }

    let data_dir = bigtiny_rust::env_contract::resolve_data_dir();
    let mut config = bigtiny_rust::config::BigTinyConfig::default();
    bigtiny_rust::env_contract::apply_env_overrides(&mut config);

    let db_path = data_dir.join("bigtiny.db").to_string_lossy().into_owned();
    let recipes_dir = bigtiny_rust::env_contract::shellexpand_home(&config.recipes.directory);

    // Port 0 and wait to be told which one we got, rather than picking a free
    // port and racing the listener to bind it.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let options = bigtiny_rust::RunOptions {
        host: "127.0.0.1".to_string(),
        port: 0,
        db_path,
        secret: Some(secret.clone()),
        // D25. See this module's header.
        require_secret: true,
        recipes_dir,
        data_dir: data_dir.to_string_lossy().into_owned(),
        encryption_key: Some(encryption_key),
        ready_tx: Some(ready_tx),
        // No shutdown channel: the daemon's lifetime is the app process's.
        // Android stops us by killing the process, and a graceful teardown we
        // never get to run is not worth the plumbing to hold.
        shutdown: None,
    };

    tauri::async_runtime::spawn(async move {
        if let Err(e) = bigtiny_rust::run(config, options).await {
            tracing::error!("embedded BigTiny daemon exited with error: {e}");
        }
    });

    // Bounded: a daemon that hasn't bound in 30s isn't going to, and the
    // caller needs an answer either way. `healthy: false` is a real state
    // downstream already handles (see `DaemonHandle::healthy`), not a
    // failure to report here.
    let addr = tokio::time::timeout(std::time::Duration::from_secs(30), ready_rx)
        .await
        .map_err(|_| "embedded BigTiny daemon did not bind within 30s".to_string())?
        .map_err(|_| "embedded BigTiny daemon stopped before binding".to_string())?;
    let port = addr.port();
    tracing::info!(port, "embedded BigTiny daemon listening");

    let client = crate::util::http_client();
    let mut healthy = false;
    for _ in 0..60 {
        if crate::lifecycle::bigtiny_proc::probe_health(&client, port).await {
            healthy = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    Ok(DaemonHandle {
        // Nothing to kill: there is no child. `ManagedProcess::default()` is
        // `owned: false` with no handle, so the teardown path correctly does
        // nothing rather than trying to signal a process that doesn't exist.
        process: ManagedProcess::default(),
        port: Some(port),
        secret_key: Some(secret),
        healthy,
    })
}
