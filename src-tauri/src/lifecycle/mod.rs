//! Process lifecycle & health for the local stack.
//!
//! We *own* the stack: on startup we spawn the BigTiny daemon, which hosts the
//! in-process inference engine. A 5s health loop recomputes the
//! [`crate::state::StackStatus`] and emits `stack://status` on change. On exit
//! we kill only the children we spawned.
//!
//! There is exactly one child now. Kitty used to also spawn and supervise
//! `ollama serve`; that ended when the engine moved in-process (docs/ANDROID.md
//! Phase 2b). An Ollama server the *user* runs is still a perfectly good
//! provider endpoint — Kitty just doesn't manage its lifecycle.

// Android hosts the daemon in-process; desktop spawns it. Both go through
// the same `BIGTINY_*` pairs in `bigtiny_env`, so the two hosts cannot drift.
#[cfg(target_os = "android")]
// Android only, per its own module doc: it hosts the daemon in-process
// because Android 10+ refuses to `exec()` a binary in app-writable storage.
// Gated rather than merely unused since Phase 7 -- desktop no longer shares
// any code with it, so compiling it there only produced dead-code warnings
// for the Credential Manager key path that is now Android's alone.
#[cfg(target_os = "android")]
pub mod bigtiny_embedded;
// Both hosts register with the V2 daemon for an app key, so this is not gated.
pub mod bigtiny_app_key;
pub mod bigtiny_env;
pub mod bigtiny_proc;
#[cfg(not(target_os = "android"))]
pub mod bigtiny_v2;
pub(crate) mod app_events;
pub(crate) mod embedding;
pub mod engine_restart;
mod health;
pub mod memory;

pub(crate) use health::{compute_status, current_payload};
pub use health::{spawn_health_loop, StackStatusPayload};

use tauri::{AppHandle, Emitter, Manager};

use crate::state::{AppState, StartupPhase};

/// Payload for the `stack://startup-phase` event.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StartupPhasePayload {
    pub phase: StartupPhase,
}

/// Publishes `Ready` when it drops, however the startup task ended.
///
/// The chat view renders "Starting…" for as long as the phase is not `Ready`,
/// and the phase was previously set by a single statement near the end of a
/// long serial startup sequence. Anything that stopped that sequence short —
/// a panic on a poisoned mutex, an early return, a step that simply never came
/// back — left the phase at `SpawningBackend` for the life of the process, and
/// the user looking at "Starting…" with no way to tell whether it was still
/// trying. A failed startup is a state the rest of the app already handles
/// (`StackStatus::BackendDown`, surfaced by the health loop); a startup that
/// never reports at all is not.
///
/// So `Ready` here means "startup is no longer in progress", not "startup
/// succeeded". Whether the stack actually works is the health loop's question.
struct StartupPhaseGuard {
    app: AppHandle,
}

impl Drop for StartupPhaseGuard {
    fn drop(&mut self) {
        set_startup_phase(&self.app, StartupPhase::Ready);
    }
}

fn set_startup_phase(app: &AppHandle, phase: StartupPhase) {
    let changed = {
        let state = app.state::<AppState>();
        // Recover from poisoning rather than propagating it: this runs inside
        // a `Drop`, and panicking there while already unwinding aborts the
        // process. The value is a single enum — the worst a half-finished
        // write can leave behind is the phase we are about to overwrite.
        let mut cur = state
            .startup_phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *cur != phase {
            *cur = phase;
            true
        } else {
            false
        }
    };
    if changed {
        if let Err(e) = app.emit("stack://startup-phase", StartupPhasePayload { phase }) {
            tracing::warn!("emit stack://startup-phase failed: {e}");
        }
    }
}

/// Sync the bundled MCP servers now if the just-spawned daemon's own startup
/// health probe already succeeded; otherwise wait for it in the background
/// instead of syncing against a daemon that (per `bigtiny_proc::spawn`'s
/// bounded wait) hasn't finished binding yet.
///
/// Calling `ensure_builtin_servers` unconditionally right after `spawn`
/// used to mean: if the daemon was still mid-`connect_all()` (every enabled
/// MCP server, including a onefile exe re-extracting under AV scanning, with
/// a 60s per-server timeout — easily longer than `spawn`'s own 15s probe
/// window), `list_servers` would fail once and the entire sync gave up for
/// the rest of the session. Retrying here, off the critical path, means a
/// slow-but-successful startup still ends with Brave/etc. registered instead
/// of silently missing until the user finds Settings → Setup & Repair.
pub(crate) fn sync_mcp_once_healthy(app: &AppHandle, healthy: bool, port: u16) {
    if healthy {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            crate::bigtiny::mcp::ensure_builtin_servers(&app).await;
            // Once, on the first healthy start: fill the subagent denylist from
            // the catalog's premium tier so an expensive model is denied before
            // a bill rather than after one. Deliberately after the daemon is up,
            // because it needs the provider list this app actually has.
            crate::commands::seed_subagent_denylist(&app).await;
        });
        return;
    }

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let client = crate::util::http_client();
        // Bounded to a few minutes total — well past any realistic onefile
        // self-extraction stall, but not forever: if the daemon truly never
        // comes up, the regular 5s health loop (`spawn_health_loop`) is what
        // surfaces the degraded `stack://status` to the user.
        for _ in 0..120 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if bigtiny_proc::probe_health(&client, port).await {
                crate::bigtiny::mcp::ensure_builtin_servers(&app).await;
                return;
            }
        }
        tracing::warn!(
            "bigtiny never answered its health check after startup; builtin MCP servers were not synced this session"
        );
    });
}

/// Warm the OpenRouter model catalog cache (provider-add redesign) at
/// startup: load the disk copy synchronously — fast, a few hundred KB — so
/// it's available to the very first Add/Edit Provider click even before
/// this task's own network fetch lands, then refresh it in the background.
/// Independent of the BigTiny daemon spawn below; runs concurrently with
/// it, not blocking or blocked by it. Failure is silent (logged only) — see
/// `openrouter::catalog::ensure_catalog_fresh`'s doc comment on why a
/// failed fetch never surfaces to the user.
fn warm_openrouter_catalog(app: &AppHandle) {
    let refetch = {
        let state = app.state::<AppState>();
        let disk = crate::openrouter::catalog::load_disk_cache();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let refetch = crate::openrouter::catalog::startup_should_refetch(disk.as_ref(), now);
        if let Some(disk) = disk {
            *state.openrouter_catalog.lock().unwrap() = Some(disk);
        }
        refetch
    };
    // On Android a recent enough cache is served as-is: see
    // `startup_should_refetch`. The lazy refresh at the point of use
    // (`ensure_catalog_fresh`, opening the model picker) still applies, so
    // anyone who actually looks at the catalog gets current data.
    if !refetch {
        tracing::info!("OpenRouter catalog cache is fresh enough; skipping the startup fetch");
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        match crate::openrouter::catalog::fetch_catalog(None).await {
            Ok(fresh) => {
                let _ = crate::openrouter::catalog::save_disk_cache(&fresh);
                let state = app.state::<AppState>();
                *state.openrouter_catalog.lock().unwrap() = Some(fresh);
            }
            Err(e) => {
                tracing::warn!("OpenRouter catalog startup fetch failed: {e}");
            }
        }
    });
}

/// Start the stack in the background at app startup. Non-blocking: failures
/// surface through the health loop as a degraded status rather than crashing.
pub fn start_stack(app: &AppHandle) {
    warm_openrouter_catalog(app);
    // Health first, daemon second. The loop's whole job is to report what the
    // stack is actually doing, and hanging that off the *end* of the daemon
    // boot meant the one situation where the user most needed a status — a
    // boot that stalled or died — was the one where no status was ever
    // published. It costs nothing to have it running before there is anything
    // to report: with no port yet it reads `BackendDown`, and the two-tick
    // debounce (see `debounce_status`) already absorbs the normal startup
    // window without flashing a degradation at a stack that is merely still
    // coming up.
    spawn_health_loop(app.clone());
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // Publishes `Ready` on every exit path from this task, including a
        // panic — see `StartupPhaseGuard`.
        let _phase = StartupPhaseGuard { app: app.clone() };
        // Before anything reads the provider list: one card, one model, and
        // no card of a retired type left as the default.
        crate::config::providers::migrate_multi_model_profiles(&app).await;
        {
            let state = app.state::<AppState>();
            let mut cfg = state.config.lock().unwrap();
            if crate::config::providers::migrate_local_profiles(&mut cfg) {
                if let Err(e) = crate::config::save(&cfg) {
                    tracing::warn!("could not save the retired provider cards: {e}");
                }
            }
        }
        // Spawn the BigTiny daemon. No provider env vars — providers are
        // registered at runtime over REST (`install_handle`).
        let snap = {
            let state = app.state::<AppState>();
            let cfg = state.config.lock().unwrap();
            crate::lifecycle::bigtiny_env::SpawnSnapshot::from_config(&cfg)
        };

        set_startup_phase(&app, StartupPhase::SpawningBackend);

        // Bundled LiteRT resources (Gemma `tokenizer.json` + the runtime DLLs) —
        // see `bigtiny_env::locate_litert_resources`'s doc comment for the
        // Windows `resource_dir()` nuance this must account for.
        let (tokenizer_path, litert_lib_dir) =
            crate::lifecycle::bigtiny_env::locate_litert_resources(&app);

        // Android links the daemon in and starts it here; desktop spawns the
        // bundled executable. Both produce the same `DaemonHandle`, so
        // everything below this point is platform-agnostic (docs/ANDROID.md
        // D8, §2.3).
        #[cfg(target_os = "android")]
        let spawn_result = {
            // Android finds `libLiteRt.so` in the APK `jniLibs`, not via PATH.
            let _ = &litert_lib_dir;
            bigtiny_embedded::start(&snap, &tokenizer_path).await
        };
        // Desktop attaches to a shared V2 daemon rather than spawning one it
        // owns -- see `bigtiny_v2`.
        #[cfg(not(target_os = "android"))]
        let spawn_result =
            bigtiny_v2::locate(&snap, &tokenizer_path, Some(litert_lib_dir.as_str())).await;
        match spawn_result {
            Ok(handle) => {
                tracing::info!(
                    "bigtiny started (health probe answered: {})",
                    handle.healthy
                );
                install_handle(&app, handle).await;
            }
            Err(e) => {
                tracing::warn!("bigtiny spawn failed: {e}");
                *app.state::<AppState>().startup_error.lock().unwrap() = Some(e);
            }
        }

        // Report whether the pathway engine's embedding GGUF is on disk, so
        // Settings can say so immediately rather than waiting up to 30s for
        // the health loop's first check. Reporting only: a missing model is
        // never downloaded behind the user's back.
        let (ap_enabled, ap_embedding_model) = {
            let state = app.state::<AppState>();
            let cfg = state.config.lock().unwrap();
            (
                cfg.adaptive_pathway_enabled,
                cfg.adaptive_pathway_embedding_model.clone(),
            )
        };
        if ap_enabled {
            embedding::refresh_embedding_status(&app, &ap_embedding_model);
        }

        // The health loop and the scheduler are already running — started
        // before this task, so a stalled boot still reports (see
        // `start_stack`). Per-provider (Personal/Remote) reachability is not
        // speculatively polled at all (Round-3 item 19, revised); it is
        // derived from real send outcomes in `commands::send_prompt` (see
        // `providers::emit_health_from_send_result`), since this app makes no
        // inference calls of its own and a background ping had no upside a
        // failed send doesn't already give us.
    });
}

/// Make `handle` the daemon Kitty talks to, and bring it up to date with
/// what Kitty expects to find there.
///
/// The one path every attach goes through - first start, a restart, and the
/// health loop finding the engine again after a crash - so a daemon Kitty
/// re-attaches to is set up exactly like one it started with.
pub(crate) async fn install_handle(app: &AppHandle, handle: crate::state::DaemonHandle) {
    let (healthy, port) = (handle.healthy, handle.port);
    {
        let state = app.state::<AppState>();
        *state.bigtiny.lock().unwrap() = handle;
        *state.startup_error.lock().unwrap() = None;
    }
    // Register the providers so the very first send has one to route to.
    if let Err(e) = crate::bigtiny::providers::sync_all_providers(app).await {
        tracing::warn!("bigtiny provider sync failed: {e}");
    }
    // Self-heal the bundled plugins' MCP-server registrations (command path
    // across an update/reinstall, enabled state matching Settings) —
    // deferred to the background if the daemon's own startup probe hasn't
    // succeeded yet, so a slow (but eventually successful) boot doesn't give
    // up on the sync after one failed `list_servers` call.
    if let Some(port) = port {
        sync_mcp_once_healthy(app, healthy, port);
    }
    // Memory follows the toggles and what this engine loaded.
    memory::apply_memory_plugins(app).await;
    // Tasks an older Kitty scheduled itself move to the engine, once.
    crate::commands::migrate_config_tasks(app).await;
    // Approvals and background runs for every chat, on screen or not.
    app_events::ensure_running(app);
}

/// Find the engine again (desktop): attach to the daemon that is up now, or
/// start one if none is. Records why on failure, for the status detail.
#[cfg(not(target_os = "android"))]
pub(crate) async fn attach_daemon(app: &AppHandle) -> Result<(), String> {
    let snap = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        crate::lifecycle::bigtiny_env::SpawnSnapshot::from_config(&cfg)
    };
    let (tokenizer_path, litert_lib_dir) =
        crate::lifecycle::bigtiny_env::locate_litert_resources(app);
    match bigtiny_v2::locate(&snap, &tokenizer_path, Some(litert_lib_dir.as_str())).await {
        Ok(handle) => {
            install_handle(app, handle).await;
            Ok(())
        }
        Err(e) => {
            *app.state::<AppState>().startup_error.lock().unwrap() = Some(e.clone());
            Err(e)
        }
    }
}

/// The health loop's recovery path: [`attach_daemon`], unless a restart or
/// another re-attach is already doing it.
#[cfg(not(target_os = "android"))]
pub(crate) async fn reattach(app: &AppHandle) {
    let state = app.state::<AppState>();
    if state
        .restart_in_progress
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_err()
    {
        return;
    }
    tracing::info!("the engine is not answering; looking for it again");
    if let Err(e) = attach_daemon(app).await {
        tracing::warn!("re-attaching to the engine failed: {e}");
    }
    state
        .restart_in_progress
        .store(false, std::sync::atomic::Ordering::SeqCst);
}

/// Kill child processes we spawned. Called on app exit.
///
/// Note: this only runs on a *graceful* exit (Tauri's `RunEvent::Exit`, wired
/// in `lib.rs`). It does NOT run when the process is terminated directly at
/// the OS level instead — a terminal Ctrl+C during `tauri dev`, or tauri-cli's
/// own dev-mode hot-restart, both kill the previous run outright rather than
/// through this event loop. `bigtiny_proc`'s own `kill_stale_orphan` (run at
/// the top of every `spawn`) is the recovery path for children orphaned that
/// way — this function alone isn't sufficient on its own.
pub fn shutdown(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut bigtiny = state.bigtiny.lock().unwrap();
    // On desktop `owned` is now always false: the daemon is a shared machine
    // resource and another app may be mid-turn, so Kitty exiting must not end
    // it. `kill_if_owned` therefore does nothing for BigTiny here, and is kept
    // for the Android in-process host and any other child this app spawns.
    // The daemon decides its own lifetime through its idle-exit timer.
    //
    // No pidfile to remove either: Kitty no longer tracks a daemon PID,
    // because it no longer claims the right to kill one.
    bigtiny.process.kill_if_owned();
    drop(bigtiny);
    tracing::info!("stack shut down");
}
