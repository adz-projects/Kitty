//! Health loops: the local stack's 5s `StackStatus` recomputation, and the
//! Adaptive Pathway sidecar's separate 5s probe + schism/embedding refresh.

use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use super::adaptive_pathway_proc::{self, AdaptivePathwayStatus, EmbeddingModelStatus};
use super::embedding::{ensure_embedding_model, set_embedding_status};
use super::ollama_proc;
use crate::state::{AppState, StackStatus};

/// Payload for the `stack://status` event.
#[derive(Debug, Clone, Serialize)]
pub struct StackStatusPayload {
    pub status: StackStatus,
    /// Optional human-readable hint (e.g. a version-mismatch note).
    pub detail: Option<String>,
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
            // Self-heal the bundled MCP servers (notably `adaptive-pathway`)
            // on a slower cadence than the schism check — every ~2 minutes —
            // so a builtin whose stdio process dropped after startup (or whose
            // `AP_SIDECAR_PORT` became stale) reconnects on its own instead of
            // silently losing its tools from the LLM tool list until the app
            // restarts. `ensure_builtin_servers` issues a `Connect` for any
            // enabled-but-not-connected row; when everything is healthy it's a
            // couple of no-op REST reads. Gated on `up` so a dead sidecar
            // doesn't make the MCP proxy spin uselessly.
            if up && tick % 24 == 0 {
                let port = app
                    .state::<AppState>()
                    .bigtiny
                    .lock()
                    .unwrap()
                    .port;
                if let Some(port) = port {
                    // Only bother when the daemon (not just the sidecar) is up.
                    if super::bigtiny_proc::probe_health(&client, port).await {
                        let app2 = app.clone();
                        tauri::async_runtime::spawn(async move {
                            crate::bigtiny::mcp::self_heal_builtin_servers(&app2).await;
                        });
                    }
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
        // Debounce (mirrors the Adaptive Pathway sidecar's own health-loop
        // debounce): a degraded reading must repeat on the *next* tick before
        // it's actually stored/reported. A single missed/slow probe (OS
        // scheduling jitter, a momentary daemon hiccup) shouldn't flip the
        // whole app into a "degraded" banner for one blip. Recovering to
        // `Ok` stays immediate; only degradation needs the extra
        // confirmation tick.
        let mut pending_degraded: Option<StackStatus> = None;
        // ~30s cadence (every 6th 5s tick), same as the Adaptive Pathway
        // health loop's embedding-model recheck: catches the summarizer
        // model being deleted out-of-band, or Ollama coming up after
        // `start_stack`'s one-time `ensure_summarizer_model` call already
        // found it unreachable.
        let mut tick: u64 = 0;
        loop {
            ticker.tick().await;
            if tick % 6 == 0 {
                let (ollama_base, summarizer) = {
                    let state = app.state::<AppState>();
                    let cfg = state.config.lock().unwrap();
                    (cfg.ollama_base_url.clone(), cfg.summarizer.clone())
                };
                if summarizer.enabled {
                    super::ensure_summarizer_model(app.clone(), ollama_base, summarizer.model)
                        .await;
                }
            }
            tick = tick.wrapping_add(1);
            let computed = compute_status(&app, &client).await;
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
                        "Kitty needs attention",
                        "The local stack is degraded. Open Kitty to fix it.",
                        None,
                    );
                }
            }
        }
    });
}

pub(crate) async fn compute_status(app: &AppHandle, client: &reqwest::Client) -> StackStatus {
    let (base, needs_ollama, bigtiny_port) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        let bigtiny_port = state.bigtiny.lock().unwrap().port;
        (
            cfg.ollama_base_url.clone(),
            cfg.ollama_enabled && ollama_proc::requires_local_ollama(&cfg),
            bigtiny_port,
        )
    };

    // Ollama reachability/model checks only apply when the active setup
    // actually needs Ollama — a remote/API-key provider shouldn't misreport
    // as broken just because no local Ollama is running (wizard redesign).
    if needs_ollama && !ollama_proc::probe_version(client, &base).await {
        return StackStatus::OllamaDown;
    }
    // `/api/health` is a real protocol-level probe (the FastAPI app
    // answering, not just a bound TCP listener).
    match bigtiny_port {
        Some(port) if super::bigtiny_proc::probe_health(client, port).await => {}
        _ => return StackStatus::BackendDown,
    }
    if needs_ollama && !ollama_proc::has_any_model(client, &base).await {
        return StackStatus::NoModel;
    }
    StackStatus::Ok
}
