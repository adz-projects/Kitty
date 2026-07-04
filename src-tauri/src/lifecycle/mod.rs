//! Process lifecycle & health for the local stack (Ollama + goosed).
//!
//! We *own* the stack: on startup we ensure Ollama is running (spawning it only
//! if it isn't already), then spawn `goose serve` (ACP-over-HTTP). A 5s health
//! loop recomputes the [`StackStatus`] and emits `stack://status` on change.
//! On exit we kill only the children we spawned.

pub mod conflict;
pub mod goosed;
pub mod ollama_proc;

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::state::AppState;

/// A child process we may or may not own. We only kill processes we spawned.
#[derive(Default)]
pub struct ManagedProcess {
    pub child: Option<std::process::Child>,
    /// True when we spawned it and must clean it up on exit.
    pub owned: bool,
}

impl ManagedProcess {
    /// Kill + reap the child, but only if we own it (never touch pre-existing
    /// user/service processes).
    pub fn kill_if_owned(&mut self) {
        if self.owned {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

/// Machine-readable stack status driving the "Fix this" UI (CLAUDE.md rule 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StackStatus {
    /// Startup, before the first health probe resolves.
    #[default]
    Starting,
    Ok,
    OllamaDown,
    GoosedDown,
    NoModel,
    ProviderUnreachable,
    ConflictGooseDesktop,
}

/// Payload for the `stack://status` event.
#[derive(Debug, Clone, Serialize)]
pub struct StackStatusPayload {
    pub status: StackStatus,
    /// Optional human-readable hint (e.g. a version-mismatch note).
    pub detail: Option<String>,
}

/// Start the stack in the background at app startup. Non-blocking: failures
/// surface through the health loop as a degraded status rather than crashing.
pub fn start_stack(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // 1. Ensure Ollama is reachable (spawn it only if down and installed).
        let base = {
            let state = app.state::<AppState>();
            let cfg = state.config.lock().unwrap();
            cfg.ollama_base_url.clone()
        };
        match ollama_proc::ensure_running(&base).await {
            Ok(proc) => {
                let state = app.state::<AppState>();
                *state.ollama.lock().unwrap() = proc;
            }
            Err(e) => tracing::warn!("ollama ensure_running failed: {e}"),
        }

        // 2. Spawn goosed (`goose serve`) with the active provider's env.
        let env = {
            let state = app.state::<AppState>();
            let cfg = state.config.lock().unwrap();
            crate::config::providers::goosed_env(&cfg)
        };
        match goosed::spawn(env).await {
            Ok(handle) => {
                let state = app.state::<AppState>();
                *state.goosed.lock().unwrap() = handle;
                tracing::info!("goosed started");
            }
            Err(e) => tracing::warn!("goosed spawn failed: {e}"),
        }

        // 3. Begin the periodic health loop.
        spawn_health_loop(app);
    });
}

/// Recompute status every 5s and emit `stack://status` when it changes.
pub fn spawn_health_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .expect("reqwest client");
        let mut ticker = tokio::time::interval(Duration::from_secs(5));
        loop {
            ticker.tick().await;
            let status = compute_status(&app, &client).await;
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

async fn compute_status(app: &AppHandle, client: &reqwest::Client) -> StackStatus {
    let (base, goosed_port, our_child_pid) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        let goosed = state.goosed.lock().unwrap();
        let pid = goosed
            .process
            .child
            .as_ref()
            .map(|c| c.id())
            .filter(|_| goosed.process.owned);
        (cfg.ollama_base_url.clone(), goosed.port, pid)
    };

    // Degraded states take precedence over the (non-blocking) conflict warning.
    if !ollama_proc::probe_version(client, &base).await {
        return StackStatus::OllamaDown;
    }
    match goosed_port {
        Some(port) if goosed::is_up(port).await => {}
        _ => return StackStatus::GoosedDown,
    }
    if !ollama_proc::has_any_model(client, &base).await {
        return StackStatus::NoModel;
    }
    if conflict::goose_desktop_running(our_child_pid) {
        return StackStatus::ConflictGooseDesktop;
    }
    StackStatus::Ok
}

/// Kill child processes we spawned. Called on app exit.
pub fn shutdown(app: &AppHandle) {
    let state = app.state::<AppState>();
    state.goosed.lock().unwrap().process.kill_if_owned();
    state.ollama.lock().unwrap().kill_if_owned();
    tracing::info!("stack shut down (owned children killed)");
}
