//! Process lifecycle & health for the local stack (Ollama + goosed).
//!
//! We *own* the stack: on startup we ensure Ollama is running (spawning it only
//! if it isn't already), then spawn `goose serve` (ACP-over-HTTP). A 5s health
//! loop recomputes the [`crate::state::StackStatus`] and emits `stack://status`
//! on change. On exit we kill only the children we spawned.

pub mod adaptive_pathway_proc;
pub mod conflict;
mod embedding;
pub mod goosed;
mod health;
pub mod ollama_proc;
pub mod scheduler;

pub(crate) use embedding::ensure_embedding_model;
pub(crate) use health::compute_status;
pub use health::{spawn_adaptive_pathway_health_loop, spawn_health_loop};

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
/// *chat* setup needs it (local Ollama provider), or adaptive-pathway is
/// enabled — AP's embeddings are local-Ollama-only regardless of chat
/// provider, so an API-key chat user still needs Ollama running purely for
/// embeddings. Independent of `ollama_enabled` ("run local chat model" — a
/// separate, narrower toggle) by design; extracted as a pure function so the
/// policy is unit testable without spinning up `start_stack`'s async runtime.
pub(crate) fn stack_needs_ollama(cfg: &crate::config::Config) -> bool {
    ollama_proc::requires_local_ollama(cfg) || cfg.adaptive_pathway_enabled
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

        // 2 + 2b. Spawn goosed (`goose serve`) with the active provider's env,
        // and — if the active provider is a local Ollama model — warm it into
        // memory (Round-2 item 5), in parallel: the two are independent I/O
        // waits (spawning a process vs. an HTTP round trip to Ollama), so
        // running them concurrently drops wall-clock startup from
        // `spawn + warmup` to `max(spawn, warmup)`. Both lookups happen
        // up front so neither async block needs to re-lock config mid-flight.
        let (env, goose_override, warm) = {
            let state = app.state::<AppState>();
            let cfg = state.config.lock().unwrap();
            (
                crate::config::providers::goosed_env(&cfg),
                cfg.goose_binary_override.clone(),
                crate::config::providers::active_ollama_target(&cfg),
            )
        };
        set_startup_phase(&app, StartupPhase::SpawningGoosed);
        if warm.is_some() {
            set_startup_phase(&app, StartupPhase::WarmingModel);
        }
        let goosed_fut = goosed::spawn(env, goose_override.as_deref());
        let warm_fut = async {
            if let Some((base, model)) = warm {
                crate::ollama::keep_alive_load(&base, &model).await;
            }
        };
        let (goosed_result, ()) = tokio::join!(goosed_fut, warm_fut);
        match goosed_result {
            Ok(handle) => {
                let state = app.state::<AppState>();
                *state.goosed.lock().unwrap() = handle;
                tracing::info!("goosed started");
            }
            Err(e) => tracing::warn!("goosed spawn failed: {e}"),
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

            // Existing-install migration: an extension registered before
            // `AP_EMBED_OLLAMA_MODEL`/`AP_EMBED_OLLAMA_URL` existed (or
            // before the URL var was added) never picks up a new `env_keys`
            // entry — that only takes effect at registration time, and
            // pre-existing installs' `config.yaml` entries never get
            // re-registered on their own. Writing the values directly into
            // the extension's own `envs:` map is a safe, idempotent fix (a
            // model tag/URL is not a secret) that runs on every launch, so
            // any existing install self-heals without the user touching the
            // Settings toggle at all.
            if let Err(e) = crate::goose_config::set_extension_env(
                "adaptive-pathway",
                "AP_EMBED_OLLAMA_MODEL",
                &ap_embedding_model,
            ) {
                tracing::warn!("adaptive pathway env migration (model) failed: {e}");
            }
            if let Err(e) = crate::goose_config::set_extension_env(
                "adaptive-pathway",
                "AP_EMBED_OLLAMA_URL",
                &base,
            ) {
                tracing::warn!("adaptive pathway env migration (url) failed: {e}");
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
/// through this event loop. `adaptive_pathway_proc::kill_stale_orphan` (run at
/// the top of every `ensure_running`) is the recovery path for children
/// orphaned that way — this function alone isn't sufficient on its own.
pub fn shutdown(app: &AppHandle) {
    let state = app.state::<AppState>();
    state.goosed.lock().unwrap().process.kill_if_owned();
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
}
