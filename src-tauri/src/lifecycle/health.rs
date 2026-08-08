//! Health loops: the local stack's 5s `StackStatus` recomputation, which
//! also hosts the builtin-MCP self-heal and the pathway embedding-model
//! refresh (neither is specific to any one feature, so both live here
//! rather than as a separate loop).

use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use super::embedding::{ensure_embedding_model, set_embedding_status, EmbeddingModelStatus};
use super::ollama_proc;
use crate::state::{AppState, StackStatus};

/// Payload for the `stack://status` event.
#[derive(Debug, Clone, Serialize)]
pub struct StackStatusPayload {
    pub status: StackStatus,
    /// Optional human-readable hint (e.g. a version-mismatch note).
    pub detail: Option<String>,
}

/// Recompute status every 5s and emit `stack://status` when it changes.
pub fn spawn_health_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let client = crate::util::http_client();
        let mut ticker = tokio::time::interval(Duration::from_secs(5));
        // Debounce: a degraded reading must repeat on the *next* tick before
        // it's actually stored/reported. A single missed/slow probe (OS
        // scheduling jitter, a momentary daemon hiccup) shouldn't flip the
        // whole app into a "degraded" banner for one blip. Recovering to
        // `Ok` stays immediate; only degradation needs the extra
        // confirmation tick.
        let mut pending_degraded: Option<StackStatus> = None;
        // ~30s cadence (every 6th 5s tick): catches the summarizer model
        // being deleted out-of-band, or Ollama coming up after
        // `start_stack`'s one-time `ensure_summarizer_model` call already
        // found it unreachable. The pathway embedding-model recheck below
        // shares this cadence for the same reason.
        let mut tick: u64 = 0;
        // Throttles the embedding auto-retry pull below: at most once per 10
        // minutes, so a sustained failure (Ollama installed but broken, no
        // disk space, no network) doesn't spam pull attempts forever.
        let mut last_pull_attempt: Option<Instant> = None;
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

                // Pathway embedding-model presence refresh. Never overwrites
                // `Downloading`: that state is only ever
                // set/cleared by the in-flight `ensure_embedding_model` pull
                // task itself; this is purely a periodic Present/Missing
                // refresh (catches a model deleted out-of-band, or Ollama
                // coming back up later).
                let (pathway_enabled, ollama_base, embedding_model) = {
                    let state = app.state::<AppState>();
                    let cfg = state.config.lock().unwrap();
                    (
                        cfg.adaptive_pathway_enabled,
                        cfg.ollama_base_url.clone(),
                        cfg.adaptive_pathway_embedding_model.clone(),
                    )
                };
                if !pathway_enabled {
                    set_embedding_status(&app, EmbeddingModelStatus::Unknown);
                } else {
                    let currently_downloading = {
                        let state = app.state::<AppState>();
                        let status = *state.adaptive_pathway_embedding_status.lock().unwrap();
                        status == EmbeddingModelStatus::Downloading
                    };
                    if !currently_downloading {
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
                        // model at launch, came up later" hole — without
                        // this, the pull only ever fires once, at
                        // `start_stack` time.
                        if ollama_up && !present {
                            let should_retry = last_pull_attempt
                                .map(|t| t.elapsed() >= Duration::from_secs(600))
                                .unwrap_or(true);
                            if should_retry {
                                last_pull_attempt = Some(Instant::now());
                                ensure_embedding_model(app.clone(), ollama_base, embedding_model)
                                    .await;
                            }
                        }
                    }
                }
            }
            // Self-heal the bundled MCP servers on a slower cadence — every
            // ~2 minutes — so a builtin whose stdio process dropped after
            // startup reconnects on its own instead of silently losing its
            // tools from the LLM tool list until the app restarts.
            // `ensure_builtin_servers` issues a `Connect` for any
            // enabled-but-not-connected row; when everything is healthy it's
            // a couple of no-op REST reads. Unconditional on the pathway
            // feature. Gated only on the daemon itself being reachable, so a down
            // BigTiny doesn't make the MCP proxy spin uselessly.
            if tick % 24 == 0 {
                let port = app.state::<AppState>().bigtiny.lock().unwrap().port;
                if let Some(port) = port {
                    if super::bigtiny_proc::probe_health(&client, port).await {
                        let app2 = app.clone();
                        tauri::async_runtime::spawn(async move {
                            crate::bigtiny::mcp::self_heal_builtin_servers(&app2).await;
                        });
                    }
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
