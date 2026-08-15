//! Restart the daemon when a load-time engine setting changes (§6.4, D11).
//!
//! Every `[local]` knob reaches BigTiny as a `BIGTINY_LOCAL__*` env var at
//! spawn (`bigtiny_proc::spawn`), so there is no in-process path to apply one
//! — changing `n_ctx` means restarting the daemon, full stop. That makes the
//! *timing* the whole design:
//!
//! - **Idle → restart immediately.** Nothing is lost.
//! - **Mid-generation → queue it.** Restarting kills the daemon, which drops
//!   the SSE stream and the turn with it. §4.1's "in-flight streams are never
//!   aborted" is not a nicety here: the user would watch a half-written reply
//!   vanish because they nudged a slider in another window.
//!
//! The in-flight signal is Kitty's own `AppState::in_flight_sessions`, which
//! `bigtiny::stream` already maintains around every turn — no round-trip to
//! the daemon to ask, and no new bookkeeping to drift out of sync.
//!
//! This is also why `reload_required`/`restart_pending` are Kitty-side rather
//! than fields on the daemon's `/api/health`, as §3.1 originally sketched: the
//! daemon is the thing being restarted and cannot know a setting changed.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::config::{Config, LocalModelSettings};
use crate::state::AppState;

/// Payload for `engine://restart-state`. Drives the non-blocking
/// "restarting…" chip — never a modal (§6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct EngineRestartState {
    /// A load-time setting changed and the running daemon no longer matches
    /// the saved config.
    pub reload_required: bool,
    /// The restart is waiting for an in-flight generation to finish.
    pub restart_pending: bool,
}

/// Everything that only reaches the daemon at spawn time. Compared as a unit
/// rather than field-by-field: the question is never "did `n_ctx` change" but
/// "is the running daemon still consistent with the saved config", and a
/// field added to `LocalModelSettings` later is load-time by construction —
/// it would be relayed by the same env block.
fn load_time_fingerprint(cfg: &Config) -> (LocalModelSettings, String, String) {
    (
        cfg.local.clone(),
        // The GGUF ids resolve to `BIGTINY_LOCAL__MODEL_PATH` /
        // `__EMBED_MODEL_PATH` at spawn, so switching model is load-time too.
        cfg.summarizer.model.clone(),
        cfg.adaptive_pathway_embedding_model.clone(),
    )
}

/// True when `new` needs a daemon restart to take effect.
pub fn needs_restart(old: &Config, new: &Config) -> bool {
    load_time_fingerprint(old) != load_time_fingerprint(new)
}

fn emit(app: &AppHandle, state: EngineRestartState) {
    let changed = {
        let s = app.state::<AppState>();
        let mut cur = s.engine_restart.lock().unwrap();
        let changed = *cur != state;
        *cur = state;
        changed
    };
    if changed {
        let _ = app.emit("engine://restart-state", state);
    }
}

/// Current state, for a window that attaches after the event fired.
pub fn current(app: &AppHandle) -> EngineRestartState {
    *app.state::<AppState>().engine_restart.lock().unwrap()
}

/// Call after persisting a config change. Restarts the daemon now if idle,
/// or marks it pending so [`apply_if_pending`] picks it up when the last
/// in-flight turn finishes.
pub fn schedule(app: &AppHandle) {
    emit(
        app,
        EngineRestartState {
            reload_required: true,
            restart_pending: false,
        },
    );
    apply_if_pending(app);
}

/// Restart now if a reload is outstanding and nothing is generating.
///
/// Safe to call from anywhere and often — it's a no-op unless a reload is
/// actually outstanding. `bigtiny::stream` calls it as each turn completes,
/// which is what drains a queued restart.
pub fn apply_if_pending(app: &AppHandle) {
    let state = current(app);
    if !state.reload_required {
        return;
    }

    let busy = {
        let s = app.state::<AppState>();
        let in_flight = s.in_flight_sessions.lock().unwrap();
        !in_flight.is_empty()
    };
    if busy {
        emit(
            app,
            EngineRestartState {
                reload_required: true,
                restart_pending: true,
            },
        );
        tracing::info!("engine restart queued behind an in-flight generation");
        return;
    }

    // Clear the flags *before* spawning rather than after the restart
    // returns: a second `schedule` landing while the restart is in flight
    // must be able to re-arm, and leaving `reload_required` set would make
    // every completing turn try to restart again.
    emit(app, EngineRestartState::default());

    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        // Re-check immediately before killing: the in-flight read above and
        // the actual restart are separated by a task hop, and a prompt that
        // started in that gap would otherwise have its daemon killed
        // mid-turn — exactly what §6.4 exists to prevent. If a session went
        // in flight meanwhile, put the flags back; its completion re-drives
        // `apply_if_pending`.
        let busy_now = {
            let s = app2.state::<AppState>();
            let in_flight = s.in_flight_sessions.lock().unwrap();
            !in_flight.is_empty()
        };
        if busy_now {
            tracing::info!(
                "engine restart re-queued: a generation went in flight while the restart was being scheduled"
            );
            emit(
                &app2,
                EngineRestartState {
                    reload_required: true,
                    restart_pending: true,
                },
            );
            return;
        }
        tracing::info!("restarting the daemon to apply changed engine settings");
        if let Err(e) = crate::commands::restart_backend(app2.clone()).await {
            tracing::warn!("engine restart failed: {e}");
            // Re-arm rather than silently pretending it applied — the running
            // daemon still doesn't match the saved config, and the chip
            // should keep saying so.
            emit(
                &app2,
                EngineRestartState {
                    reload_required: true,
                    restart_pending: false,
                },
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only load-time settings trigger a restart. Anything else saved in the
    /// same `set_config` call — a theme, a hotkey, a folder — must not kill
    /// the daemon.
    #[test]
    fn unrelated_settings_do_not_trigger_a_restart() {
        let a = Config::default();
        let b = Config {
            theme: "dark".into(),
            strict_remote_mode: true,
            show_artifacts: false,
            ..Config::default()
        };
        assert!(!needs_restart(&a, &b));
    }

    #[test]
    fn a_changed_engine_knob_triggers_a_restart() {
        let a = Config::default();
        for mutate in [
            (|c: &mut Config| c.local.n_ctx = 8192) as fn(&mut Config),
            |c: &mut Config| c.local.n_gpu_layers = 0,
            |c: &mut Config| c.local.n_batch = 1024,
            |c: &mut Config| c.local.n_threads = 4,
            |c: &mut Config| c.local.cache_type_k = "q8_0".into(),
            |c: &mut Config| c.local.cache_type_v = "q8_0".into(),
            |c: &mut Config| c.local.backend = "cpu".into(),
            |c: &mut Config| c.local.embed_n_ctx = 1024,
            |c: &mut Config| c.local.embed_pooling = "mean".into(),
        ] {
            let mut b = Config::default();
            mutate(&mut b);
            assert!(
                needs_restart(&a, &b),
                "expected a restart for {:?}",
                b.local
            );
        }
    }

    /// Switching either model is load-time too: the GGUF ids resolve to
    /// `BIGTINY_LOCAL__*_MODEL_PATH` at spawn, so a running daemon keeps the
    /// old weights until it's replaced.
    #[test]
    fn switching_either_model_triggers_a_restart() {
        let a = Config::default();

        let b = Config {
            summarizer: crate::config::SummarizerSettings {
                model: "some-other-model".into(),
                ..Default::default()
            },
            ..Config::default()
        };
        assert!(needs_restart(&a, &b));

        let c = Config {
            adaptive_pathway_embedding_model: "bge-small-en-v1.5".into(),
            ..Config::default()
        };
        assert!(needs_restart(&a, &c));
    }

    /// Saving the same config twice (the UI does this on every keystroke in
    /// some panels) must not restart the daemon repeatedly.
    #[test]
    fn an_unchanged_config_never_restarts() {
        let a = Config::default();
        assert!(!needs_restart(&a, &a.clone()));
    }
}
