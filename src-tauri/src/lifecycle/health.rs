//! Health loops: the local stack's 5s `StackStatus` recomputation, which
//! also hosts the builtin-MCP self-heal and the pathway embedding-model
//! refresh (neither is specific to any one feature, so both live here
//! rather than as a separate loop).

use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use super::embedding::{refresh_embedding_status, set_embedding_status, EmbeddingModelStatus};
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
        // Debounce: a degraded reading must appear on two *consecutive* ticks
        // before it's actually stored/reported. A single missed/slow probe
        // (OS scheduling jitter, a momentary daemon hiccup) shouldn't flip
        // the whole app into a "degraded" banner for one blip. Recovering to
        // `Ok` stays immediate; only degradation needs the extra
        // confirmation tick. See `debounce_status`.
        let mut degraded_streak: u32 = 0;
        // ~30s cadence (every 6th 5s tick): catches the pathway embedding
        // model being deleted out-of-band.
        let mut tick: u64 = 0;
        loop {
            ticker.tick().await;
            if tick % 6 == 0 {
                // Pathway embedding-model presence refresh. Never overwrites
                // `Downloading`: that state belongs to an in-flight download
                // task; this is purely a periodic Present/Missing refresh.
                //
                // There is no auto-retry loop any more. The old one existed
                // because the model arrived via `ollama pull`, which could
                // fail transiently and needed re-attempting on a throttle.
                // A GGUF is either on disk or it isn't — re-checking a
                // filesystem path is the whole job, and downloading is now an
                // explicit user action in Settings → Local Models.
                let (pathway_enabled, embedding_model) = {
                    let state = app.state::<AppState>();
                    let cfg = state.config.lock().unwrap();
                    (
                        cfg.adaptive_pathway_enabled,
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
                        refresh_embedding_status(&app, &embedding_model);
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
            if self_heal_due(tick) {
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
            let Some(status) = debounce_status(&mut degraded_streak, computed) else {
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
    let (needs_local_model, bigtiny_port) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        let bigtiny_port = state.bigtiny.lock().unwrap().port;
        (super::stack_needs_local_model(&cfg), bigtiny_port)
    };

    // `/api/health` is a real protocol-level probe (the daemon answering, not
    // just a bound TCP listener). Checked first: without the backend, whether
    // a model file exists is moot.
    match bigtiny_port {
        Some(port) if super::bigtiny_proc::probe_health(client, port).await => {}
        _ => return StackStatus::BackendDown,
    }
    // Only applies when the active setup actually needs a local model — a
    // remote/API-key provider shouldn't misreport as broken just because no
    // GGUF has been downloaded.
    if needs_local_model && crate::models::installed().is_empty() {
        return StackStatus::LocalModelMissing;
    }
    StackStatus::Ok
}

/// Debounce gate for the health loop: publish a degradation only after two
/// *consecutive* non-Ok readings — of any flavor. The old rule required the
/// *identical* degraded status twice in a row, so a stack flapping between
/// two different degradations (e.g. `BackendDown` ↔ `LocalModelMissing`)
/// never published anything at all. Recovering to `Ok` publishes immediately
/// and resets the streak.
fn debounce_status(non_ok_streak: &mut u32, computed: StackStatus) -> Option<StackStatus> {
    if computed == StackStatus::Ok {
        *non_ok_streak = 0;
        return Some(computed);
    }
    *non_ok_streak += 1;
    if *non_ok_streak >= 2 {
        *non_ok_streak = 0;
        Some(computed)
    } else {
        None
    }
}

/// `tick 0` is deliberately excluded: `tokio::time::interval` completes its
/// first tick *immediately*, which used to run the MCP self-heal's
/// `ensure_builtin_servers` at startup in a race with
/// `sync_mcp_once_healthy`'s own pass — both list-then-create the builtin
/// rows, and `mcp_servers.name` has no UNIQUE constraint, so the race left
/// permanent duplicates. The first self-heal now runs at ~2 minutes
/// (`tick == 24`). `ensure_builtin_servers` is additionally Mutex-serialized
/// as defense in depth (see `bigtiny::mcp`); keep both.
fn self_heal_due(tick: u64) -> bool {
    tick > 0 && tick % 24 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_publishes_ok_immediately_and_resets_the_streak() {
        let mut streak = 0;
        assert_eq!(debounce_status(&mut streak, StackStatus::Ok), Some(StackStatus::Ok));
        assert_eq!(streak, 0);
    }

    #[test]
    fn debounce_holds_a_single_degraded_blip() {
        let mut streak = 0;
        assert_eq!(debounce_status(&mut streak, StackStatus::BackendDown), None);
        // One blip followed by a recovery: nothing degraded was ever published.
        assert_eq!(debounce_status(&mut streak, StackStatus::Ok), Some(StackStatus::Ok));
        assert_eq!(streak, 0);
    }

    /// Regression (815bugs #17): the old identical-twice rule never published
    /// a stack flapping between two *different* degraded states.
    #[test]
    fn debounce_publishes_on_the_second_consecutive_non_ok_even_when_it_differs() {
        let mut streak = 0;
        assert_eq!(debounce_status(&mut streak, StackStatus::BackendDown), None);
        assert_eq!(
            debounce_status(&mut streak, StackStatus::LocalModelMissing),
            Some(StackStatus::LocalModelMissing)
        );
        assert_eq!(streak, 0, "the streak resets once published");
    }

    #[test]
    fn self_heal_never_runs_on_tick_zero() {
        assert!(!self_heal_due(0));
        assert!(!self_heal_due(23));
        assert!(self_heal_due(24));
        assert!(self_heal_due(48));
    }
}
