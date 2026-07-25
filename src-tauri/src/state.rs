//! Managed application state: window-agnostic runtime data shared across commands.
//!
//! Holds the loaded config plus the process/health machinery (BigTiny +
//! Ollama handles, generated secret/port, current stack status).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;
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
    BackendDown,
    NoModel,
    ProviderUnreachable,
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
    SpawningBackend,
    /// Only entered when a local Ollama model needs warming (see
    /// `config::providers::active_ollama_target`) — remote/API-key providers
    /// skip straight from `SpawningBackend` to `Ready`.
    WarmingModel,
    Ready,
}

/// Root managed state, registered via `app.manage(AppState::new(..))`.
pub struct AppState {
    /// App configuration (persisted to `%APPDATA%/Kitty/config.json`).
    pub config: Mutex<Config>,
    /// The Ollama process — only `Some(child)` when *we* started it.
    pub ollama: Mutex<ManagedProcess>,
    /// Last computed stack status, so the health loop only emits on change.
    pub stack_status: Mutex<StackStatus>,
    /// One-time startup progress (see `StartupPhase`); set by `start_stack`,
    /// read by `get_startup_phase` for late-attaching windows.
    pub startup_phase: Mutex<StartupPhase>,
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
    /// The BigTiny daemon child we spawn, plus its port + secret (sent as
    /// `X-API-Key`).
    pub bigtiny: Mutex<DaemonHandle>,
    /// BigTiny pending tool approvals: action_id -> session_id. The frontend
    /// only echoes back the tool_call_id (= the action id), but BigTiny's
    /// `/approve` endpoint is per-session, so the session must be remembered
    /// here between `hitl_pause` and the user's response.
    pub bigtiny_approvals: Mutex<HashMap<String, String>>,
    /// Session ids with a turn currently in flight — lets a window adopting a
    /// session (Expand mid-stream, or just resuming one another window/
    /// process is actively driving) know a turn is still running, since a
    /// resume's replay alone doesn't reliably convey that. Checked fresh at
    /// adoption time rather than trusting a client-captured snapshot, since
    /// the turn can finish in the gap between handoff and adoption.
    pub in_flight_sessions: Mutex<HashSet<String>>,
    /// Every currently-open chat window (the classic singleton `"main"` plus
    /// any number of dynamically-allocated `chat-N` windows), label -> bound
    /// session id (`None` until the window's own session-creation lazily
    /// assigns one on first send, or an Expand handoff assigns it
    /// immediately). Additive bookkeeping for multi-window support — distinct
    /// from `active_session` above, which is older, unrelated state still
    /// used by the provider context-handoff gate (Settings -> Providers).
    pub chat_windows: Mutex<HashMap<String, Option<String>>>,
    /// Monotonic counter for allocating fresh chat window labels (`chat-1`,
    /// `chat-2`, ...). Reset every launch — labels only need to be unique
    /// among currently-open windows, not across restarts.
    pub next_chat_window_id: Mutex<u64>,
    /// One-shot handoff payload for a newly-created chat window (Expand from
    /// the overlay), keyed by that window's own label and consumed (removed)
    /// the first time the window reads it. Targeted by label rather than
    /// broadcast to every window, unlike `active_session`/`session://active`
    /// — with N chat windows possibly open, a broadcast would race every one
    /// of them into adopting the same handoff.
    pub pending_handoffs: Mutex<HashMap<String, Value>>,
    /// Feature 3 (screenshot capture) — the downsampled preview + virtual-
    /// screen rect for the currently-open selection window to read once on
    /// mount (`get_screenshot_preview`), and the channel its selection (or
    /// cancellation) is delivered back through to the awaiting
    /// `capture_screenshot_region` command. A single global slot, not keyed
    /// by window label: only one screenshot capture can be visually in
    /// flight at a time (the user can't interact with two selection
    /// overlays at once), so there's nothing to disambiguate.
    pub screenshot_preview: Mutex<Option<(String, i32, i32, i32, i32)>>,
    pub screenshot_selection: Mutex<Option<tokio::sync::oneshot::Sender<Option<(i32, i32, i32, i32)>>>>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        Self {
            config: Mutex::new(config),
            ollama: Mutex::new(ManagedProcess::default()),
            stack_status: Mutex::new(StackStatus::default()),
            startup_phase: Mutex::new(StartupPhase::default()),
            active_session: Mutex::new(None),
            settings_target: Mutex::new(None),
            wizard_mode: Mutex::new(None),
            adaptive_pathway: Mutex::new(ManagedProcess::default()),
            adaptive_pathway_status: Mutex::new(AdaptivePathwayStatus::default()),
            adaptive_pathway_embedding_status: Mutex::new(EmbeddingModelStatus::default()),
            bigtiny: Mutex::new(DaemonHandle::default()),
            bigtiny_approvals: Mutex::new(HashMap::new()),
            in_flight_sessions: Mutex::new(HashSet::new()),
            chat_windows: Mutex::new(HashMap::new()),
            next_chat_window_id: Mutex::new(0),
            pending_handoffs: Mutex::new(HashMap::new()),
            screenshot_preview: Mutex::new(None),
            screenshot_selection: Mutex::new(None),
        }
    }
}

/// Connection details for the BigTiny daemon we manage.
#[derive(Default)]
pub struct DaemonHandle {
    pub process: ManagedProcess,
    pub port: Option<u16>,
    /// Sent as `X-API-Key` on every BigTiny request.
    pub secret_key: Option<String>,
}
