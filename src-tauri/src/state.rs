//! Managed application state: window-agnostic runtime data shared across commands.
//!
//! Holds the loaded config plus the process/health machinery (the BigTiny
//! daemon handle, generated secret/port, current stack status).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;
use crate::lifecycle::embedding::EmbeddingModelStatus;

/// A physical-pixel screen rectangle `(x, y, width, height)`, shared by the
/// screenshot preview + selection plumbing — keeps the long tuple out of the
/// `AppState` field and command-signature types.
pub type ScreenshotRegion = (i32, i32, i32, i32);

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
    BackendDown,
    /// The active setup wants a local model and none is installed. Replaces
    /// the old `OllamaDown`/`NoModel` pair: with no managed inference process
    /// there is nothing to be "down", so the only local failure left is a
    /// missing GGUF — which Settings → Local Models can actually fix.
    LocalModelMissing,
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
    Ready,
}

/// Root managed state, registered via `app.manage(AppState::new(..))`.
pub struct AppState {
    /// App configuration (persisted to `%APPDATA%/Kitty/config.json`).
    pub config: Mutex<Config>,
    /// One-time corrupt-config recovery marker: `Some(backup_path)` when a
    /// `config.json` failed to parse at startup and was backed up to
    /// `config.json.corrupt-<timestamp>` before the app fell back to
    /// defaults. The frontend reads it once via
    /// `get_config_recovery_notice` to show that saved settings were reset.
    pub config_recovered: Mutex<Option<String>>,
    /// Last computed stack status, so the health loop only emits on change.
    pub stack_status: Mutex<StackStatus>,
    /// Whether a load-time engine setting changed since the daemon spawned,
    /// and whether applying it is waiting on an in-flight generation
    /// (docs/ANDROID.md §6.4). See `lifecycle::engine_restart`.
    pub engine_restart: Mutex<crate::lifecycle::engine_restart::EngineRestartState>,
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
    /// Readiness of the embedding GGUF the in-process pathway engine uses —
    /// `Downloading`/`Missing` degrades gracefully to the engine's hashing
    /// fallback, not an outage. See `EmbeddingModelStatus`.
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
    /// Written only by the Win32 GDI capture path (`crate::screenshot`), so
    /// unread on non-Windows targets — the field stays so `AppState` has one
    /// shape everywhere.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub screenshot_preview: Mutex<Option<(String, ScreenshotRegion)>>,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub screenshot_selection: Mutex<Option<tokio::sync::oneshot::Sender<Option<ScreenshotRegion>>>>,
    /// Last-seen `compacted_through_rowid` per session, from BigTiny's
    /// `/api/chat/{id}/stats`. BigTiny's own background compaction pass
    /// runs fire-and-forget from the *end* of a turn (see
    /// `bigtiny/agent/loop.py`'s `finally` block) and typically finishes
    /// after this turn's own SSE stream has already closed, so its
    /// `compaction` SSE event usually has no open connection left to
    /// deliver on. `stream::send_prompt` instead polls stats once, shortly
    /// after emitting `chat://complete`, and diffs against the value
    /// stored here to decide whether to emit `chat://compaction` — this
    /// map is what makes that diff possible across polls.
    pub bigtiny_compaction_watermarks: Mutex<HashMap<String, i64>>,
    /// Window labels whose frontend has confirmed it mounted (dev-only
    /// load watchdog, see `windows::spawn_load_watchdog`). A window's first
    /// navigation in dev goes over HTTP to the Vite server; if that ever
    /// errors or times out, nothing else in `windows.rs` would retry it, so
    /// this is what lets the watchdog tell "still loading" apart from
    /// "never going to load".
    pub booted_windows: Mutex<HashSet<String>>,
}

impl AppState {
    /// `config_recovered`: `Some(backup_path)` when the loaded config had to
    /// be recovered from a corrupt `config.json` (see
    /// `config::load_with_recovery`) — surfaced once to the frontend as a
    /// recovery notice.
    pub fn new(config: Config, config_recovered: Option<String>) -> Self {
        Self {
            config: Mutex::new(config),
            config_recovered: Mutex::new(config_recovered),
            stack_status: Mutex::new(StackStatus::default()),
            engine_restart: Mutex::new(Default::default()),
            startup_phase: Mutex::new(StartupPhase::default()),
            active_session: Mutex::new(None),
            settings_target: Mutex::new(None),
            wizard_mode: Mutex::new(None),
            adaptive_pathway_embedding_status: Mutex::new(EmbeddingModelStatus::default()),
            bigtiny: Mutex::new(DaemonHandle::default()),
            bigtiny_approvals: Mutex::new(HashMap::new()),
            in_flight_sessions: Mutex::new(HashSet::new()),
            chat_windows: Mutex::new(HashMap::new()),
            next_chat_window_id: Mutex::new(0),
            pending_handoffs: Mutex::new(HashMap::new()),
            screenshot_preview: Mutex::new(None),
            screenshot_selection: Mutex::new(None),
            bigtiny_compaction_watermarks: Mutex::new(HashMap::new()),
            booted_windows: Mutex::new(HashSet::new()),
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
    /// Whether `spawn`'s own startup health probe ever got a response within
    /// its bounded wait. `false` doesn't mean the daemon is dead — a Python
    /// interpreter + FastAPI import chain (plus `connect_all()`-ing every
    /// enabled MCP server before uvicorn even binds) can outlast that
    /// window — but it does mean callers must not assume `/api/mcp/servers`
    /// is reachable yet. See `lifecycle::sync_mcp_once_healthy`.
    pub healthy: bool,
}
