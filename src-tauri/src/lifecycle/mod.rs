//! Process lifecycle & health for the local stack (Ollama + BigTiny).
//!
//! We *own* the stack: on startup we ensure Ollama is running (spawning it only
//! if it isn't already), then spawn the BigTiny daemon. A 5s health loop
//! recomputes the [`crate::state::StackStatus`] and emits `stack://status`
//! on change. On exit we kill only the children we spawned.

pub mod adaptive_pathway_proc;
pub mod bigtiny_proc;
mod embedding;
mod health;
pub mod ollama_proc;
pub mod scheduler;
mod summarizer_model;

pub(crate) use embedding::ensure_embedding_model;
pub(crate) use health::compute_status;
pub use health::{spawn_adaptive_pathway_health_loop, spawn_health_loop};
pub(crate) use summarizer_model::ensure_summarizer_model;

use adaptive_pathway_proc::AdaptivePathwayStatus;

use tauri::{AppHandle, Emitter, Manager};

use crate::state::{AppState, StartupPhase};

/// Payload for the `stack://startup-phase` event.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StartupPhasePayload {
    pub phase: StartupPhase,
}

fn set_startup_phase(app: &AppHandle, phase: StartupPhase) {
    let changed = {
        let state = app.state::<AppState>();
        let mut cur = state.startup_phase.lock().unwrap();
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

/// Whether Ollama must be started for the current config: either the active
/// *chat* setup needs it (local Ollama provider), adaptive-pathway is
/// enabled (its embeddings are local-Ollama-only regardless of chat
/// provider), or the summarizer is enabled (its model — see
/// `Config::summarizer` — is likewise always pulled from local Ollama,
/// independent of the main chat provider). Independent of `ollama_enabled`
/// ("run local chat model" — a separate, narrower toggle) by design;
/// extracted as a pure function so the policy is unit testable without
/// spinning up `start_stack`'s async runtime.
pub(crate) fn stack_needs_ollama(cfg: &crate::config::Config) -> bool {
    ollama_proc::requires_local_ollama(cfg) || cfg.adaptive_pathway_enabled || cfg.summarizer.enabled
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

/// Start the stack in the background at app startup. Non-blocking: failures
/// surface through the health loop as a degraded status rather than crashing.
pub fn start_stack(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // 1. Ensure Ollama is reachable (spawn it only if down and installed).
        // Needed either when the active *chat* setup requires it (local
        // Ollama provider), or whenever adaptive-pathway is enabled — AP's
        // embeddings are local-Ollama-only regardless of chat provider (an
        // API-key chat user still needs Ollama for embeddings), so its need
        // is independent of the `ollama_enabled` "run local chat model" flag.
        let (base, needs_ollama) = {
            let state = app.state::<AppState>();
            let cfg = state.config.lock().unwrap();
            (cfg.ollama_base_url.clone(), stack_needs_ollama(&cfg))
        };
        if needs_ollama {
            match ollama_proc::ensure_running(&base).await {
                Ok(proc) => {
                    let state = app.state::<AppState>();
                    *state.ollama.lock().unwrap() = proc;
                }
                Err(e) => tracing::warn!("ollama ensure_running failed: {e}"),
            }
        }

        // Spawn the BigTiny daemon. No provider env vars — providers are
        // registered at runtime over REST (see
        // `bigtiny::providers::sync_active_provider` right after spawn).
        let (command, args, dir, warm, summarizer, token_management) = {
            let state = app.state::<AppState>();
            let cfg = state.config.lock().unwrap();
            (
                cfg.bigtiny_command.clone(),
                cfg.bigtiny_args.clone(),
                cfg.bigtiny_dir.clone(),
                crate::config::providers::active_ollama_target(&cfg),
                cfg.summarizer.clone(),
                cfg.token_management.clone(),
            )
        };

        // Runtime guarantee for BigTiny's summarizer model (see
        // `Config::summarizer`, e.g. `qwen3.5:0.8b`): if Ollama is reachable
        // but the pinned tag isn't installed yet, pull it in the background
        // — non-blocking, doesn't delay the BigTiny spawn below (BigTiny only
        // needs the model once it actually runs a summarization pass, not to
        // start). Progress flows through the existing `ollama://pull-progress`
        // events (fixed pull id, see `summarizer_model::SUMMARIZER_MODEL_PULL_ID`).
        if summarizer.enabled {
            ensure_summarizer_model(app.clone(), base.clone(), summarizer.model.clone()).await;
        }

        set_startup_phase(&app, StartupPhase::SpawningBackend);
        if warm.is_some() {
            set_startup_phase(&app, StartupPhase::WarmingModel);
        }
        let spawn_fut =
            bigtiny_proc::spawn(&command, &args, dir.as_deref(), &summarizer, &token_management);
        let warm_fut = async {
            if let Some((base, model)) = warm {
                crate::ollama::keep_alive_load(&base, &model).await;
            }
        };
        let (spawn_result, ()) = tokio::join!(spawn_fut, warm_fut);
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
        set_startup_phase(&app, StartupPhase::Ready);

        // 2c. Adaptive Pathway extension sidecar (optional, off by default —
        // see `adaptive_pathway_proc`). Probe-then-spawn, same as Ollama: never
        // touch a pre-existing instance.
        let (
            ap_enabled,
            ap_launch_command,
            ap_launch_args,
            ap_db_path,
            ap_port,
            ap_embedding_model,
        ) = {
            let state = app.state::<AppState>();
            let cfg = state.config.lock().unwrap();
            (
                cfg.adaptive_pathway_enabled,
                cfg.adaptive_pathway_launch_command.clone(),
                cfg.adaptive_pathway_launch_args.clone(),
                cfg.adaptive_pathway_db_path.clone(),
                cfg.adaptive_pathway_port,
                cfg.adaptive_pathway_embedding_model.clone(),
            )
        };
        if ap_enabled {
            *app.state::<AppState>()
                .adaptive_pathway_status
                .lock()
                .unwrap() = AdaptivePathwayStatus::Starting;
            match adaptive_pathway_proc::ensure_running(
                &ap_launch_command,
                &ap_launch_args,
                &ap_db_path,
                ap_port,
                &ap_embedding_model,
                &base,
            )
            .await
            {
                Ok(proc) => {
                    let base = format!("http://127.0.0.1:{ap_port}");
                    let client = crate::util::http_client();
                    let up = adaptive_pathway_proc::probe_health(&client, &base).await;
                    let state = app.state::<AppState>();
                    *state.adaptive_pathway.lock().unwrap() = proc;
                    *state.adaptive_pathway_status.lock().unwrap() = if up {
                        AdaptivePathwayStatus::Ok
                    } else {
                        AdaptivePathwayStatus::Down
                    };
                }
                Err(e) => {
                    tracing::warn!("adaptive pathway sidecar ensure_running failed: {e}");
                    *app.state::<AppState>()
                        .adaptive_pathway_status
                        .lock()
                        .unwrap() = AdaptivePathwayStatus::Down;
                }
            }

            // 2d. Runtime guarantee for the shared embedding model: if Ollama
            // is reachable but the pinned tag isn't installed yet, pull it in
            // the background (non-blocking — the wizard's own pull is only
            // best-effort, so this is what actually guarantees convergence on
            // every launch, not just first run). Progress flows through the
            // existing `ollama://pull-progress` events (fixed `pull_id` below
            // so a Settings UI can subscribe to it deterministically) plus the
            // new `adaptive_pathway://embedding_status` event.
            ensure_embedding_model(app.clone(), base.clone(), ap_embedding_model.clone()).await;
        }

        // 3. Begin the local stack's health loop. Per-provider (Personal/Remote)
        // reachability is no longer speculatively polled (Round-3 item 19, revised) —
        // it's derived from real send outcomes in `commands::send_prompt` instead
        // (see `providers::emit_health_from_send_result`), since this app makes no
        // inference calls of its own and a background ping had no upside a failed
        // send doesn't already give us.
        spawn_health_loop(app.clone());
        spawn_adaptive_pathway_health_loop(app.clone());
        scheduler::spawn_scheduler_loop(app.clone());
    });
}

/// Kill child processes we spawned. Called on app exit.
///
/// Note: this only runs on a *graceful* exit (Tauri's `RunEvent::Exit`, wired
/// in `lib.rs`). It does NOT run when the process is terminated directly at
/// the OS level instead — a terminal Ctrl+C during `tauri dev`, or tauri-cli's
/// own dev-mode hot-restart, both kill the previous run outright rather than
/// through this event loop. `adaptive_pathway_proc`/`bigtiny_proc`'s own
/// `kill_stale_orphan` (run at the top of every `ensure_running`/`spawn`) is
/// the recovery path for children orphaned that way — this function alone
/// isn't sufficient on its own.
pub fn shutdown(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut bigtiny = state.bigtiny.lock().unwrap();
    let bigtiny_was_owned = bigtiny.process.owned;
    bigtiny.process.kill_if_owned();
    drop(bigtiny);
    if bigtiny_was_owned {
        bigtiny_proc::remove_pidfile();
    }
    state.ollama.lock().unwrap().kill_if_owned();
    let mut ap = state.adaptive_pathway.lock().unwrap();
    let ap_was_owned = ap.owned;
    ap.kill_if_owned();
    drop(ap);
    if ap_was_owned {
        let db_path = state
            .config
            .lock()
            .unwrap()
            .adaptive_pathway_db_path
            .clone();
        adaptive_pathway_proc::remove_pidfile(&db_path);
    }
    tracing::info!("stack shut down (owned children killed)");
}

#[cfg(test)]
mod tests {
    use super::{ollama_proc, stack_needs_ollama};
    use crate::config::Config;

    #[test]
    fn needs_ollama_true_when_adaptive_pathway_enabled_even_for_api_key_provider() {
        // The key policy flip: an API-key chat user (e.g. Anthropic/OpenRouter)
        // still needs Ollama running, purely for adaptive-pathway's embeddings.
        let mut cfg = Config::default();
        cfg.providers
            .push(crate::config::providers::ProviderProfile {
                id: "p1".into(),
                name: "Claude".into(),
                provider_type: "anthropic".into(),
                base_url: "https://api.anthropic.com".into(),
                models: vec!["claude-sonnet-5".into()],
                is_trusted: true,
                temperature: None,
                top_p: None,
                context_length: None,
                strip_reasoning: false,
                system_prompt: None,
                prompt_idle_timeout_secs: None,
                created_at: "2026-01-01T00:00:00Z".into(),
            });
        cfg.active_provider_id = Some("p1".into());
        cfg.adaptive_pathway_enabled = true;
        assert!(!ollama_proc::requires_local_ollama(&cfg));
        assert!(stack_needs_ollama(&cfg));
    }

    #[test]
    fn needs_ollama_false_when_api_key_provider_and_adaptive_pathway_disabled() {
        let mut cfg = Config::default();
        cfg.providers
            .push(crate::config::providers::ProviderProfile {
                id: "p1".into(),
                name: "Claude".into(),
                provider_type: "anthropic".into(),
                base_url: "https://api.anthropic.com".into(),
                models: vec!["claude-sonnet-5".into()],
                is_trusted: true,
                temperature: None,
                top_p: None,
                context_length: None,
                strip_reasoning: false,
                system_prompt: None,
                prompt_idle_timeout_secs: None,
                created_at: "2026-01-01T00:00:00Z".into(),
            });
        cfg.active_provider_id = Some("p1".into());
        cfg.adaptive_pathway_enabled = false;
        cfg.summarizer.enabled = false;
        assert!(!stack_needs_ollama(&cfg));
    }

    #[test]
    fn needs_ollama_true_for_local_ollama_chat_provider_regardless_of_adaptive_pathway() {
        let cfg = Config {
            adaptive_pathway_enabled: false,
            ..Config::default()
        };
        // No active provider -> requires_local_ollama defaults true (fresh install).
        assert!(stack_needs_ollama(&cfg));
    }

    #[test]
    fn needs_ollama_true_when_summarizer_enabled_even_for_api_key_provider() {
        // Mirrors the adaptive-pathway case above: the summarizer's model
        // always comes from local Ollama regardless of the main chat
        // provider, so an API-key chat user still needs Ollama running
        // purely to keep the summarizer model available.
        let mut cfg = Config::default();
        cfg.providers
            .push(crate::config::providers::ProviderProfile {
                id: "p1".into(),
                name: "Claude".into(),
                provider_type: "anthropic".into(),
                base_url: "https://api.anthropic.com".into(),
                models: vec!["claude-sonnet-5".into()],
                is_trusted: true,
                temperature: None,
                top_p: None,
                context_length: None,
                strip_reasoning: false,
                system_prompt: None,
                prompt_idle_timeout_secs: None,
                created_at: "2026-01-01T00:00:00Z".into(),
            });
        cfg.active_provider_id = Some("p1".into());
        cfg.adaptive_pathway_enabled = false;
        cfg.summarizer.enabled = true;
        assert!(!ollama_proc::requires_local_ollama(&cfg));
        assert!(stack_needs_ollama(&cfg));
    }
}
