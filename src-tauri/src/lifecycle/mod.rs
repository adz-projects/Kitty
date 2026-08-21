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
pub mod bigtiny_embedded;
pub mod bigtiny_env;
pub mod bigtiny_proc;
pub(crate) mod embedding;
pub mod engine_restart;
mod health;
pub mod scheduler;

pub(crate) use health::compute_status;
pub use health::spawn_health_loop;

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

/// Whether the current config needs a local GGUF on disk: either the active
/// *chat* setup is local, adaptive-pathway is enabled (its embeddings run on
/// the in-process engine regardless of which provider serves chat — an
/// API-key chat user still needs an embedding model), or the summarizer is
/// enabled (likewise local, independent of the main chat provider).
///
/// Successor to `stack_needs_ollama`. The policy is unchanged — what changed
/// is that "needs it" no longer implies spawning anything, only that a model
/// file has to exist. Kept as a pure function so it stays unit testable
/// without `start_stack`'s async runtime.
pub(crate) fn stack_needs_local_model(cfg: &crate::config::Config) -> bool {
    requires_local_chat_model(cfg) || cfg.adaptive_pathway_enabled || cfg.summarizer.enabled
}

/// True when chat itself runs locally: no provider is active yet (fresh
/// install, which defaults to local), or the active profile is a local one.
///
/// A profile pointed at an Ollama server the *user* runs is remote as far as
/// Kitty is concerned — it needs no model of ours — so `"ollama"` counts as
/// remote here even though it didn't under managed Ollama.
fn requires_local_chat_model(cfg: &crate::config::Config) -> bool {
    match cfg
        .active_provider_id
        .as_ref()
        .and_then(|id| cfg.providers.iter().find(|p| &p.id == id))
    {
        Some(p) => p.provider_type == "local",
        None => true,
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
    scheduler::spawn_scheduler_loop(app.clone());
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // Publishes `Ready` on every exit path from this task, including a
        // panic — see `StartupPhaseGuard`.
        let _phase = StartupPhaseGuard { app: app.clone() };
        // Spawn the BigTiny daemon. No provider env vars — providers are
        // registered at runtime over REST (see
        // `bigtiny::providers::sync_active_provider` right after spawn).
        let (
            command,
            args,
            dir,
            summarizer,
            token_management,
            memory,
            local,
            pathway_enabled,
            pathway_embedding_model,
        ) = {
            let state = app.state::<AppState>();
            let cfg = state.config.lock().unwrap();
            (
                cfg.bigtiny_command.clone(),
                cfg.bigtiny_args.clone(),
                cfg.bigtiny_dir.clone(),
                cfg.summarizer.clone(),
                cfg.token_management.clone(),
                cfg.memory.clone(),
                cfg.local.clone(),
                cfg.adaptive_pathway_enabled,
                cfg.adaptive_pathway_embedding_model.clone(),
            )
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
            // The spawn-only inputs have no meaning without a child process, and
            // Android finds `libLiteRt.so` in the APK `jniLibs`, not via PATH.
            let _ = (&command, &args, &dir, &litert_lib_dir);
            bigtiny_embedded::start(
                &summarizer,
                &token_management,
                &memory,
                &local,
                pathway_enabled,
                &pathway_embedding_model,
                &tokenizer_path,
            )
            .await
        };
        #[cfg(not(target_os = "android"))]
        let spawn_result = bigtiny_proc::spawn(
            &command,
            &args,
            dir.as_deref(),
            &summarizer,
            &token_management,
            &memory,
            &local,
            pathway_enabled,
            &pathway_embedding_model,
            &tokenizer_path,
            Some(litert_lib_dir.as_str()),
        )
        .await;
        match spawn_result {
            Ok(handle) => {
                let (healthy, port) = (handle.healthy, handle.port);
                let state = app.state::<AppState>();
                *state.bigtiny.lock().unwrap() = handle;
                tracing::info!("bigtiny started (health probe answered: {healthy})");
                // Register the active provider so the very first send has
                // a healthy provider to route to.
                if let Err(e) = crate::bigtiny::providers::sync_active_provider(&app).await {
                    tracing::warn!("bigtiny provider sync failed: {e}");
                }
                // Self-heal the bundled plugins' MCP-server registrations
                // (command path across an update/reinstall, enabled state
                // matching Settings) — deferred to the background if the
                // daemon's own startup probe hasn't succeeded yet, so a slow
                // (but eventually successful) boot doesn't give up on the
                // sync after one failed `list_servers` call.
                if let Some(port) = port {
                    sync_mcp_once_healthy(&app, healthy, port);
                }
            }
            Err(e) => tracing::warn!("bigtiny spawn failed: {e}"),
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
    let bigtiny_was_owned = bigtiny.process.owned;
    bigtiny.process.kill_if_owned();
    drop(bigtiny);
    if bigtiny_was_owned {
        bigtiny_proc::remove_pidfile();
    }
    tracing::info!("stack shut down (owned children killed)");
}

#[cfg(test)]
mod tests {
    use super::{requires_local_chat_model, stack_needs_local_model};
    use crate::config::providers::ProviderProfile;
    use crate::config::Config;

    /// A config with one active provider of `provider_type`. Every other
    /// field is an unconfigured default — `ProviderProfile` has no `Default`,
    /// and inlining this literal four times was most of the old test module.
    fn cfg_with_active(provider_type: &str) -> Config {
        let mut cfg = Config::default();
        cfg.providers.push(ProviderProfile {
            id: "p1".into(),
            name: "test".into(),
            provider_type: provider_type.into(),
            base_url: "https://example.invalid".into(),
            models: vec!["some-model".into()],
            is_trusted: true,
            temperature: None,
            top_p: None,
            top_k: None,
            min_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            max_tokens: None,
            context_length: None,
            strip_reasoning: false,
            supports_vision: false,
            system_prompt: None,
            prompt_idle_timeout_secs: None,
            parallel_slots: None,
            created_at: "2026-01-01T00:00:00Z".into(),
        });
        cfg.active_provider_id = Some("p1".into());
        cfg.adaptive_pathway_enabled = false;
        cfg.summarizer.enabled = false;
        cfg
    }

    /// The policy that survived retiring managed Ollama: an API-key chat user
    /// still needs a local model, because adaptive-pathway's embeddings run
    /// locally no matter who serves chat. This was the most load-bearing of
    /// the original four assertions and it is unchanged in substance — only
    /// what "needs it" implies changed (a file on disk, not a process).
    #[test]
    fn a_local_model_is_needed_for_pathway_even_with_an_api_key_provider() {
        let mut cfg = cfg_with_active("anthropic");
        cfg.adaptive_pathway_enabled = true;
        assert!(!requires_local_chat_model(&cfg));
        assert!(stack_needs_local_model(&cfg));
    }

    /// Same shape for the summarizer, which is also local regardless of the
    /// chat provider.
    #[test]
    fn a_local_model_is_needed_for_the_summarizer_even_with_an_api_key_provider() {
        let mut cfg = cfg_with_active("anthropic");
        cfg.summarizer.enabled = true;
        assert!(!requires_local_chat_model(&cfg));
        assert!(stack_needs_local_model(&cfg));
    }

    #[test]
    fn no_local_model_is_needed_for_a_pure_api_key_setup() {
        let cfg = cfg_with_active("anthropic");
        assert!(!stack_needs_local_model(&cfg));
    }

    /// A fresh install has no active provider and defaults to local.
    #[test]
    fn a_fresh_install_needs_a_local_model() {
        let cfg = Config {
            adaptive_pathway_enabled: false,
            ..Config::default()
        };
        assert!(requires_local_chat_model(&cfg));
        assert!(stack_needs_local_model(&cfg));
    }

    #[test]
    fn a_local_chat_provider_needs_a_local_model() {
        let cfg = cfg_with_active("local");
        assert!(requires_local_chat_model(&cfg));
        assert!(stack_needs_local_model(&cfg));
    }

    /// **Changed behaviour, deliberately.** Under managed Ollama an `ollama`
    /// profile meant "the server we run for you", so it required a local
    /// model. It now means "a server you run yourself" — remote as far as
    /// Kitty is concerned, needing nothing of ours on disk.
    #[test]
    fn an_ollama_profile_is_now_treated_as_remote() {
        let cfg = cfg_with_active("ollama");
        assert!(!requires_local_chat_model(&cfg));
        assert!(!stack_needs_local_model(&cfg));
    }
}
