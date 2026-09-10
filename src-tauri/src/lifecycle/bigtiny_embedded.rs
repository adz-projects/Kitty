//! The BigTiny V2 daemon, hosted **inside** the app process (docs/ANDROID.md
//! D8, §2.1/§2.3). Android only.
//!
//! Android 10+ refuses to `exec()` anything in an app-writable directory, so
//! there is no daemon executable to spawn here and `externalBin` is empty in
//! `tauri.android.conf.json`. The daemon is linked in instead and started as a
//! library call.
//!
//! **The HTTP boundary is kept.** `bigtiny2::run` still binds a loopback
//! listener and Kitty still talks to it through `bigtiny::client`, exactly as
//! on desktop. Calling the daemon's internals directly would have been faster
//! in the narrow sense and would have forked every call site in `bigtiny/` into
//! two implementations — the streaming one especially. One wire protocol for
//! both platforms is worth a loopback hop.
//!
//! **That loopback is not private**, which is what D25 was written about: on
//! Android any app holding `INTERNET` can reach `127.0.0.1`. Under V1 this host
//! had to opt into protection by setting `require_secret`. V2 removed the
//! choice — `server::middleware::auth_middleware` denies every `/api/*` route
//! but `/api/health` unless the caller presents a key that resolves to a
//! registered app. So D25 is satisfied by construction now, and the field that
//! used to carry it is vestigial.
//!
//! **Kitty does not supply the at-rest encryption key.** V2 owns it, in
//! `{data_dir}/encryption.key`, the same as on desktop — see `bigtiny_v2` for
//! why a shared daemon cannot take its key from whichever app happened to start
//! it. That also takes the AndroidKeyStore round trip out of the startup path
//! entirely; provider keys are still sealed by that keystore, but through
//! `config::providers::keyring`, not through the daemon.

use crate::state::{DaemonHandle, ManagedProcess};

/// Start the daemon on a background task and return once it is listening and
/// Kitty has an app key for it.
///
/// Mirrors `bigtiny_v2::locate`'s contract — same `DaemonHandle`, same health
/// semantics — so everything downstream (`bigtiny::client`,
/// `sync_mcp_once_healthy`, the health loop) is identical on both platforms and
/// none of it needs to know which host it got.
///
/// Takes the same settings that function does, minus the ones that only mean
/// something for a child process, so `start_stack` reads config once and both
/// hosts are configured from identical values.
#[allow(clippy::too_many_arguments)]
pub async fn start(
    summarizer: &crate::config::SummarizerSettings,
    token_management: &crate::config::TokenManagementSettings,
    memory: &crate::config::MemorySettings,
    local: &crate::config::LocalModelSettings,
    specialists: &crate::config::SpecialistSettings,
    pathway_enabled: bool,
    pathway_embedding_model: &str,
    tokenizer_path: &str,
) -> Result<DaemonHandle, String> {
    // Under V1 this was a daemon-wide shared secret. Under V2 it is the
    // bootstrap `registration_token`: it authorizes `POST /api/apps/register`
    // and nothing else, and regenerating it per launch is correct precisely
    // because it must not outlive the daemon that issued it. The durable
    // credential is the app key that registration returns, below.
    let registration_token = crate::lifecycle::bigtiny_proc::generate_secret();

    // Same variables the desktop host passes as child-process env. Here they go
    // on *our own* process, which is also how the in-process MCP servers
    // (`mcp::builtin`) receive their configuration: `connect` takes no env map,
    // so a linked server reads the host process environment.
    //
    // Safe to set at this point specifically because it happens during startup,
    // before the daemon task exists and before any MCP server is connected —
    // `set_var` is not thread-safe against a concurrent reader.
    //
    // The empty encryption key is dropped by `daemon_env` itself: V2 treats a
    // present-but-empty `BIGTINY_ENCRYPTION_KEY` as a malformed key and refuses
    // to start.
    for (key, value) in crate::lifecycle::bigtiny_env::daemon_env(
        &registration_token,
        "",
        summarizer,
        token_management,
        memory,
        local,
        specialists,
        pathway_enabled,
        pathway_embedding_model,
        tokenizer_path,
    ) {
        std::env::set_var(key, value);
    }

    // **V2 renamed the data-dir variable.** `daemon_env` sets `BIGTINY_DATA_DIR`
    // (V1's name); `resolve_data_dir` reads `BIGTINYV2_DATA_DIR`. Unset, it
    // falls back to `dirs_home().join(".bigtiny-v2")` -- and bionic reports
    // `HOME` as `/`, so the daemon tried to create `/.bigtiny-v2` and died with
    // `Read-only file system (os error 30)` before it ever bound. Desktop never
    // saw this because the same function short-circuits to `%APPDATA%` on
    // Windows, so the missing variable is invisible there.
    //
    // A *sibling* of V1's directory rather than the same one: V2's schema is not
    // V1's, and keeping `bigtiny/` untouched is what makes the frozen V1 crate a
    // real rollback path instead of a nominal one. This is the on-device half of
    // the fresh-start decision (D26c) -- provider keys are re-entered once.
    //
    // Set here rather than in `daemon_env` because that map is shared with the
    // desktop host, where the daemon deliberately owns its own location
    // (`%APPDATA%/BigTinyV2`) and may already be running for another frontend.
    let data_dir = crate::config::config_dir()
        .map(|d| d.join("bigtiny-v2"))
        .map_err(|e| format!("no writable app directory for the BigTiny data dir: {e}"))?;
    std::env::set_var(
        bigtiny2::discovery::DATA_DIR_ENV,
        data_dir.as_os_str(),
    );

    let mut config = bigtiny2::config::BigTinyConfig::default();
    bigtiny2::env_contract::apply_env_overrides(&mut config);

    let db_path = data_dir.join("bigtiny.db").to_string_lossy().into_owned();

    // Port 0 and wait to be told which one we got, rather than picking a free
    // port and racing the listener to bind it.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let options = bigtiny2::RunOptions {
        host: "127.0.0.1".to_string(),
        port: 0,
        db_path,
        secret: Some(registration_token.clone()),
        // Vestigial in V2 — auth is unconditional. See this module's header.
        require_secret: true,
        data_dir: data_dir.to_string_lossy().into_owned(),
        // V2 owns its own key. See this module's header.
        encryption_key: None,
        ready_tx: Some(ready_tx),
        // No shutdown channel: the daemon's lifetime is the app process's.
        // Android stops us by killing the process, and a graceful teardown we
        // never get to run is not worth the plumbing to hold.
        shutdown: None,
        // The idle-exit timer exists so a *shared* daemon can decide for itself
        // when no frontend needs it any more. In-process there is exactly one
        // frontend and it is us, so the daemon exiting under a live app would
        // be a bug, not a cleanup.
        idle_exit_mins: None,
    };

    tauri::async_runtime::spawn(async move {
        if let Err(e) = bigtiny2::run(config, options).await {
            tracing::error!("embedded BigTiny daemon exited with error: {e}");
        }
    });

    // Bounded: a daemon that hasn't bound in 30s isn't going to, and the caller
    // needs an answer either way.
    let addr = tokio::time::timeout(std::time::Duration::from_secs(30), ready_rx)
        .await
        .map_err(|_| "embedded BigTiny daemon did not bind within 30s".to_string())?
        .map_err(|_| "embedded BigTiny daemon stopped before binding".to_string())?;
    let port = addr.port();
    tracing::info!(port, "embedded BigTiny daemon listening");

    // Health first, then registration: `/api/health` is the one route open
    // without a key, so it is what tells us the daemon is actually serving.
    // Registering against a listener that has bound but not finished coming up
    // would fail for a reason that has nothing to do with the credential.
    let client = crate::util::http_client();
    let mut healthy = false;
    for _ in 0..60 {
        if crate::lifecycle::bigtiny_proc::probe_health(&client, port).await {
            healthy = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    // The durable half of the identity. Unlike the token above, this survives
    // both the daemon restarting and the app relaunching — it is stored in the
    // AndroidKeyStore via `config::providers::keyring`, and lives daemon-side
    // in `apps.key_hash`.
    let base_url = format!("http://127.0.0.1:{port}");
    let app_key =
        crate::lifecycle::bigtiny_app_key::ensure_app_key(&base_url, &registration_token).await?;

    Ok(DaemonHandle {
        // Nothing to kill: there is no child. `ManagedProcess::default()` is
        // `owned: false` with no handle, so the teardown path correctly does
        // nothing rather than trying to signal a process that doesn't exist.
        process: ManagedProcess::default(),
        port: Some(port),
        // What `bigtiny::client` sends as `X-API-Key` on every request.
        secret_key: Some(app_key),
        healthy,
    })
}
