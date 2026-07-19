//! Managed application state: window-agnostic runtime data shared across commands.
//!
//! Holds the loaded config plus the process/health machinery populated in Phase 1
//! (goosed + Ollama handles, generated secret/port, current stack status).

use std::collections::HashSet;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

use crate::config::Config;
use crate::goosed::api::AcpClient;
use crate::lifecycle::adaptive_pathway_proc::{AdaptivePathwayStatus, EmbeddingModelStatus};

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

/// Transient one-time startup progress, kept separate from `StackStatus`
/// (which is a steady-state health readout re-derived every 5s and can't
/// represent "spawning" — see `lifecycle::start_stack`). Set imperatively
/// during `start_stack` and never touched by the health loop. `Ready` once
/// the app has finished its one-time startup sequence, and thereafter has no
/// further bearing on chat availability (that's `StackStatus`'s job).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StartupPhase {
    #[default]
    SpawningGoosed,
    /// Only entered when a local Ollama model needs warming (see
    /// `config::providers::active_ollama_target`) — remote/API-key providers
    /// skip straight from `SpawningGoosed` to `Ready`.
    WarmingModel,
    Ready,
}

/// Root managed state, registered via `app.manage(AppState::new(..))`.
pub struct AppState {
    /// App configuration (persisted to `%APPDATA%/goose-overlay/config.json`).
    pub config: Mutex<Config>,
    /// The goosed (`goose serve`) child we spawn, plus its port + secret key.
    pub goosed: Mutex<GoosedHandle>,
    /// The Ollama process — only `Some(child)` when *we* started it.
    pub ollama: Mutex<ManagedProcess>,
    /// Last computed stack status, so the health loop only emits on change.
    pub stack_status: Mutex<StackStatus>,
    /// One-time startup progress (see `StartupPhase`); set by `start_stack`,
    /// read by `get_startup_phase` for late-attaching windows.
    pub startup_phase: Mutex<StartupPhase>,
    /// The live ACP connection to goosed, lazily established on first use and
    /// cleared on disconnect (async mutex: held across `.await`).
    pub acp: AsyncMutex<Option<AcpClient>>,
    /// The active session (raw `SessionInfo` JSON) handed from overlay to the
    /// full window on "Expand" so both bind the same session.
    pub active_session: Mutex<Option<Value>>,
    /// Deep-link target for the settings window (`{ section, highlight }`).
    pub settings_target: Mutex<Option<Value>>,
    /// Wizard launch mode (`"setup"` or `"repair"`).
    pub wizard_mode: Mutex<Option<String>>,
    /// The Adaptive Pathway sidecar process — only `Some(child)` when *we*
    /// started it (never a pre-existing instance).
    pub adaptive_pathway: Mutex<ManagedProcess>,
    /// Last computed Adaptive Pathway sidecar status, kept separate from
    /// `stack_status` since it's an optional augmentation, not a chat-blocking
    /// dependency.
    pub adaptive_pathway_status: Mutex<AdaptivePathwayStatus>,
    /// Readiness of the shared `qwen3-embedding:0.6b` Ollama model — separate
    /// from `adaptive_pathway_status` since the sidecar can be `Ok` while this
    /// is still `Downloading`/`Missing` (hashing-fallback degradation, not an
    /// outage). See `EmbeddingModelStatus`.
    pub adaptive_pathway_embedding_status: Mutex<EmbeddingModelStatus>,
    /// Session ids with a `session/prompt` currently in flight — lets a window
    /// adopting a session (Expand mid-stream, or just resuming one another
    /// window/process is actively driving) know a turn is still running, since
    /// `session/load`'s replay alone doesn't reliably convey that. Checked
    /// fresh at adoption time rather than trusting a client-captured snapshot,
    /// since the turn can finish in the gap between handoff and adoption.
    pub in_flight_sessions: Mutex<HashSet<String>>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        Self {
            config: Mutex::new(config),
            goosed: Mutex::new(GoosedHandle::default()),
            ollama: Mutex::new(ManagedProcess::default()),
            stack_status: Mutex::new(StackStatus::default()),
            startup_phase: Mutex::new(StartupPhase::default()),
            acp: AsyncMutex::new(None),
            active_session: Mutex::new(None),
            settings_target: Mutex::new(None),
            wizard_mode: Mutex::new(None),
            adaptive_pathway: Mutex::new(ManagedProcess::default()),
            adaptive_pathway_status: Mutex::new(AdaptivePathwayStatus::default()),
            adaptive_pathway_embedding_status: Mutex::new(EmbeddingModelStatus::default()),
            in_flight_sessions: Mutex::new(HashSet::new()),
        }
    }
}

/// Connection details for the goosed ACP server we manage.
#[derive(Default)]
pub struct GoosedHandle {
    pub process: ManagedProcess,
    pub port: Option<u16>,
    /// Sent as `X-Secret-Key` on ACP requests — consumed by `goosed/api.rs` in Phase 2.
    #[allow(dead_code)]
    pub secret_key: Option<String>,
}

impl GoosedHandle {
    /// Base URL of the local ACP endpoint, if the server has been started.
    /// Used by the ACP client in Phase 2.
    #[allow(dead_code)]
    pub fn base_url(&self) -> Option<String> {
        self.port.map(|p| format!("http://127.0.0.1:{p}"))
    }
}
