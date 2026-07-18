//! Process lifecycle & health for the local stack (Ollama + goosed).
//!
//! We *own* the stack: on startup we ensure Ollama is running (spawning it only
//! if it isn't already), then spawn `goose serve` (ACP-over-HTTP). A 5s health
//! loop recomputes the [`StackStatus`] and emits `stack://status` on change.
//! On exit we kill only the children we spawned.

pub mod adaptive_pathway_proc;
pub mod conflict;
pub mod goosed;
pub mod ollama_proc;
pub mod scheduler;

use adaptive_pathway_proc::{AdaptivePathwayStatus, EmbeddingModelStatus};

use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::state::{AppState, StackStatus};

/// Payload for the `stack://status` event.
#[derive(Debug, Clone, Serialize)]
pub struct StackStatusPayload {
    pub status: StackStatus,
    /// Optional human-readable hint (e.g. a version-mismatch note).
    pub detail: Option<String>,
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

        // 2. Spawn goosed (`goose serve`) with the active provider's env.
        let (env, goose_override) = {
            let state = app.state::<AppState>();
            let cfg = state.config.lock().unwrap();
            (
                crate::config::providers::goosed_env(&cfg),
                cfg.goose_binary_override.clone(),
            )
        };
        match goosed::spawn(env, goose_override.as_deref()).await {
            Ok(handle) => {
                let state = app.state::<AppState>();
                *state.goosed.lock().unwrap() = handle;
                tracing::info!("goosed started");
            }
            Err(e) => tracing::warn!("goosed spawn failed: {e}"),
        }

        // 2b. If the active provider is a local Ollama model, warm it into memory
        // and keep it resident (Round-2 item 5).
        let warm = {
            let state = app.state::<AppState>();
            let cfg = state.config.lock().unwrap();
            crate::config::providers::active_ollama_target(&cfg)
        };
        if let Some((base, model)) = warm {
            crate::ollama::keep_alive_load(&base, &model).await;
        }

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

/// Payload for the `adaptive_pathway://status` event — only emitted on change.
#[derive(Debug, Clone, Serialize)]
pub struct AdaptivePathwayStatusPayload {
    pub status: AdaptivePathwayStatus,
}

/// Payload for the `adaptive_pathway://schism` event — only emitted when
/// `schism_state` flips into `detected`/`reviewing` (never on every poll).
#[derive(Debug, Clone, Serialize)]
pub struct AdaptivePathwaySchismPayload {
    pub state: String,
}

/// Payload for the `adaptive_pathway://embedding_status` event — only
/// emitted on change (mirrors `adaptive_pathway://status`).
#[derive(Debug, Clone, Serialize)]
pub struct AdaptivePathwayEmbeddingStatusPayload {
    pub status: EmbeddingModelStatus,
}

/// Fixed `pull_id` for the embedding-model auto-pull, so a Settings UI can
/// subscribe to `ollama://pull-progress` for this specific pull without
/// having to be told the id first (unlike the wizard's own model pulls,
/// which are user-initiated and get a fresh id each time).
const EMBEDDING_MODEL_PULL_ID: &str = "adaptive-pathway-embedding-model";

fn set_embedding_status(app: &AppHandle, status: EmbeddingModelStatus) {
    let changed = {
        let state = app.state::<AppState>();
        let mut cur = state.adaptive_pathway_embedding_status.lock().unwrap();
        if *cur != status {
            *cur = status;
            true
        } else {
            false
        }
    };
    if changed {
        let _ = app.emit(
            "adaptive_pathway://embedding_status",
            AdaptivePathwayEmbeddingStatusPayload { status },
        );
    }
}

/// Runtime guarantee (vs. the wizard's best-effort pull): if Ollama is
/// reachable but the pinned embedding-model tag isn't installed, pull it in
/// the background. Non-blocking — `start_stack` continues immediately; the
/// pull's own progress is what drives `EmbeddingModelStatus` to `Present`.
/// Safe to call whenever adaptive-pathway is enabled, including if Ollama
/// itself turns out to be unreachable (reported as `Missing`, not an error).
pub(crate) async fn ensure_embedding_model(app: AppHandle, ollama_base: String, model: String) {
    let client = crate::util::http_client();
    if !ollama_proc::probe_version(&client, &ollama_base).await {
        set_embedding_status(&app, EmbeddingModelStatus::Missing);
        return;
    }
    if ollama_proc::has_model_tag(&client, &ollama_base, &model).await {
        set_embedding_status(&app, EmbeddingModelStatus::Present);
        return;
    }
    set_embedding_status(&app, EmbeddingModelStatus::Downloading);
    tauri::async_runtime::spawn(async move {
        crate::ollama::pull_model(
            app.clone(),
            ollama_base.clone(),
            model.clone(),
            EMBEDDING_MODEL_PULL_ID.to_string(),
        )
        .await;
        // Reuse the same shared client rather than building a second one for
        // the post-pull re-check (Round-7 item 2: this task previously built
        // two clients back-to-back for no reason).
        let client = crate::util::http_client();
        let present = ollama_proc::has_model_tag(&client, &ollama_base, &model).await;
        set_embedding_status(
            &app,
            if present {
                EmbeddingModelStatus::Present
            } else {
                EmbeddingModelStatus::Missing
            },
        );
    });
}

/// Separate small loop (not merged into `spawn_health_loop`, which is
/// Ollama+goosed specific and must keep ticking even when this feature is
/// disabled): probes `/health` every 5s when enabled; every 6th tick (~30s)
/// also reads `/state` and emits `adaptive_pathway://schism` only when
/// `schism_state` flips into `detected`/`reviewing` — this is what lets the
/// Schism Resolution modal appear without the user having Settings open.
pub fn spawn_adaptive_pathway_health_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let client = crate::util::http_client();
        let mut ticker = tokio::time::interval(Duration::from_secs(5));
        let mut tick: u64 = 0;
        let mut last_schism_state = "none".to_string();
        // A single missed/slow probe (sidecar briefly busy, OS scheduling
        // jitter) shouldn't flip the status and flicker the UI — require 2
        // consecutive failures (~10s of sustained unreachability) before
        // reporting `Down`. Recovery to `Ok` stays immediate on the very
        // first successful probe; only degradation is debounced.
        let mut consecutive_failures: u32 = 0;
        // Throttles the auto-retry pull below: at most once per 10 minutes,
        // so a sustained failure (Ollama installed but broken, no disk
        // space, no network) doesn't spam pull attempts every ~30s forever.
        let mut last_pull_attempt: Option<Instant> = None;
        loop {
            ticker.tick().await;
            let (enabled, port) = {
                let state = app.state::<AppState>();
                let cfg = state.config.lock().unwrap();
                (cfg.adaptive_pathway_enabled, cfg.adaptive_pathway_port)
            };
            if !enabled {
                consecutive_failures = 0;
                let state = app.state::<AppState>();
                let mut status = state.adaptive_pathway_status.lock().unwrap();
                if *status != AdaptivePathwayStatus::Disabled {
                    *status = AdaptivePathwayStatus::Disabled;
                }
                drop(status);
                set_embedding_status(&app, EmbeddingModelStatus::Unknown);
                tick = tick.wrapping_add(1);
                continue;
            }
            let base = format!("http://127.0.0.1:{port}");
            let up = adaptive_pathway_proc::probe_health(&client, &base).await;
            if up {
                consecutive_failures = 0;
            } else {
                consecutive_failures = consecutive_failures.saturating_add(1);
            }
            // `None` means "not enough evidence to report a change yet" (a
            // first/lone failed probe) — the debounce itself, distinct from
            // `changed == false` (evidence gathered, but nothing to report).
            let observed = if up {
                Some(AdaptivePathwayStatus::Ok)
            } else if consecutive_failures >= 2 {
                Some(AdaptivePathwayStatus::Down)
            } else {
                None
            };
            let changed = observed.and_then(|new_status| {
                let state = app.state::<AppState>();
                let mut cur = state.adaptive_pathway_status.lock().unwrap();
                if *cur != new_status {
                    *cur = new_status;
                    Some(new_status)
                } else {
                    None
                }
            });
            if let Some(status) = changed {
                let _ = app.emit(
                    "adaptive_pathway://status",
                    AdaptivePathwayStatusPayload { status },
                );
            }
            if up && tick % 6 == 0 {
                if let Ok(state) = crate::adaptive_pathway::get_state(&base).await {
                    let schism_state = state
                        .get("schism_state")
                        .and_then(|v| v.as_str())
                        .unwrap_or("none")
                        .to_string();
                    let newly_active = matches!(schism_state.as_str(), "detected" | "reviewing")
                        && last_schism_state != schism_state;
                    if newly_active {
                        let _ = app.emit(
                            "adaptive_pathway://schism",
                            AdaptivePathwaySchismPayload {
                                state: schism_state.clone(),
                            },
                        );
                    }
                    last_schism_state = schism_state;
                }
            }
            // Same ~30s cadence as the schism check. Never overwrites
            // `Downloading` here — that state is only ever set/cleared by the
            // in-flight `ensure_embedding_model` pull task itself; this is
            // purely a periodic Present/Missing refresh (e.g. catches a model
            // deleted out-of-band, or Ollama coming back up later).
            if tick % 6 == 0 {
                let currently_downloading = {
                    let state = app.state::<AppState>();
                    let status = *state.adaptive_pathway_embedding_status.lock().unwrap();
                    status == EmbeddingModelStatus::Downloading
                };
                if !currently_downloading {
                    let (ollama_base, embedding_model) = {
                        let state = app.state::<AppState>();
                        let cfg = state.config.lock().unwrap();
                        (
                            cfg.ollama_base_url.clone(),
                            cfg.adaptive_pathway_embedding_model.clone(),
                        )
                    };
                    let ollama_up = ollama_proc::probe_version(&client, &ollama_base).await;
                    let present = ollama_up
                        && ollama_proc::has_model_tag(&client, &ollama_base, &embedding_model)
                            .await;
                    set_embedding_status(
                        &app,
                        if present {
                            EmbeddingModelStatus::Present
                        } else {
                            EmbeddingModelStatus::Missing
                        },
                    );
                    // Auto-retry: closes the "Ollama was down/missing the
                    // model at launch, came up later" hole — without this,
                    // the pull only ever fires once, at `start_stack` time.
                    if ollama_up && !present {
                        let should_retry = last_pull_attempt
                            .map(|t| t.elapsed() >= Duration::from_secs(600))
                            .unwrap_or(true);
                        if should_retry {
                            last_pull_attempt = Some(Instant::now());
                            ensure_embedding_model(app.clone(), ollama_base, embedding_model).await;
                        }
                    }
                }
            }
            tick = tick.wrapping_add(1);
        }
    });
}

/// Recompute status every 5s and emit `stack://status` when it changes.
pub fn spawn_health_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let client = crate::util::http_client();
        let mut ticker = tokio::time::interval(Duration::from_secs(5));
        // The Goose Desktop conflict check enumerates processes — slow-changing
        // and comparatively costly, so refresh it only every 12th tick (~60s)
        // rather than every 5s (Round-5 Batch 8). Cached between refreshes.
        let mut tick: u64 = 0;
        let mut conflict = false;
        // Debounce (mirrors the Adaptive Pathway sidecar's own health-loop
        // debounce): a degraded reading must repeat on the *next* tick before
        // it's actually stored/reported. A single missed/slow probe — e.g.
        // the brief window during a legitimate provider-switch goosed
        // restart — shouldn't flip the whole app into a "degraded" banner for
        // one blip (confirmed real report: switching providers mid-chat
        // sometimes surfaced a false "goose server is down"). Recovering to
        // `Ok` stays immediate; only degradation needs the extra
        // confirmation tick.
        let mut pending_degraded: Option<StackStatus> = None;
        loop {
            ticker.tick().await;
            if tick % 12 == 0 {
                let our_child_pid = {
                    let state = app.state::<AppState>();
                    let goosed = state.goosed.lock().unwrap();
                    goosed
                        .process
                        .child
                        .as_ref()
                        .map(|c| c.id())
                        .filter(|_| goosed.process.owned)
                };
                conflict = conflict::goose_desktop_running(our_child_pid);
            }
            tick = tick.wrapping_add(1);
            let computed = compute_status(&app, &client, conflict).await;
            let status = if computed == StackStatus::Ok || pending_degraded == Some(computed) {
                pending_degraded = None;
                computed
            } else {
                pending_degraded = Some(computed);
                continue;
            };
            let changed = {
                let state = app.state::<AppState>();
                let mut cur = state.stack_status.lock().unwrap();
                if *cur != status {
                    *cur = status;
                    true
                } else {
                    false
                }
            };
            if changed {
                let payload = StackStatusPayload {
                    status,
                    detail: None,
                };
                if let Err(e) = app.emit("stack://status", payload) {
                    tracing::warn!("emit stack://status failed: {e}");
                }
                tracing::info!("stack status -> {status:?}");
                // Notify on entering a degraded state while the overlay is hidden.
                if !matches!(status, StackStatus::Ok | StackStatus::Starting) {
                    crate::notifications::notify_if_hidden(
                        &app,
                        crate::notifications::Event::StackDegraded,
                        "Goose needs attention",
                        "The local stack is degraded. Open Goose to fix it.",
                    );
                }
            }
        }
    });
}

/// `conflict` is the cached "stock Goose Desktop running" flag, refreshed on a
/// slower cadence by the caller (Round-5 Batch 8) since it's slow-changing.
pub(crate) async fn compute_status(
    app: &AppHandle,
    client: &reqwest::Client,
    conflict: bool,
) -> StackStatus {
    let (base, goosed_port, needs_ollama) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        let goosed = state.goosed.lock().unwrap();
        (
            cfg.ollama_base_url.clone(),
            goosed.port,
            cfg.ollama_enabled && ollama_proc::requires_local_ollama(&cfg),
        )
    };

    // Degraded states take precedence over the (non-blocking) conflict warning.
    // Ollama reachability/model checks only apply when the active setup
    // actually needs Ollama — a remote/API-key provider shouldn't misreport
    // as broken just because no local Ollama is running (wizard redesign).
    if needs_ollama && !ollama_proc::probe_version(client, &base).await {
        return StackStatus::OllamaDown;
    }
    match goosed_port {
        Some(port) if goosed::is_up(port).await => {
            // A bare TCP connect only confirms the OS-level listener is still
            // bound — not that the ACP protocol layer inside goosed is
            // actually responding. Confirmed real gap: the ACP WebSocket
            // closed/reconnected three times in one real session with zero
            // degraded-banner/notification, because this was the *only*
            // goosed check and it stayed trivially true the whole time even
            // while the protocol layer itself was flaking. A short, capped
            // `session/list` round trip actually exercises that layer —
            // capped independently of `request()`'s own (much longer,
            // 300s) internal timeout so one slow/wedged probe can't stall
            // this loop's own 5s cadence.
            let acp_ok = match crate::goosed::api::ensure_client(app).await {
                Ok(client) => tokio::time::timeout(
                    Duration::from_secs(3),
                    client.request("session/list", serde_json::json!({})),
                )
                .await
                .map(|r| r.is_ok())
                .unwrap_or(false),
                Err(_) => false,
            };
            if !acp_ok {
                return StackStatus::GoosedDown;
            }
        }
        _ => return StackStatus::GoosedDown,
    }
    if needs_ollama && !ollama_proc::has_any_model(client, &base).await {
        return StackStatus::NoModel;
    }
    if conflict {
        return StackStatus::ConflictGooseDesktop;
    }
    StackStatus::Ok
}

/// Kill child processes we spawned. Called on app exit.
pub fn shutdown(app: &AppHandle) {
    let state = app.state::<AppState>();
    state.goosed.lock().unwrap().process.kill_if_owned();
    state.ollama.lock().unwrap().kill_if_owned();
    state.adaptive_pathway.lock().unwrap().kill_if_owned();
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
