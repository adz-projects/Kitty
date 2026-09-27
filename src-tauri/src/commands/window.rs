//! Window-lifecycle and backend-process commands: overlay/settings/main show,
//! stack status, and the BigTiny restart used by "Fix this" + provider switches.

use tauri::AppHandle;

use crate::lifecycle;
use crate::state::AppState;
use crate::state::StartupPhase;
use crate::windows;

/// Hide the overlay (Escape handler in the overlay UI calls this).
#[tauri::command]
pub fn hide_overlay(app: AppHandle) -> Result<(), String> {
    windows::hide_overlay(&app).map_err(|e| e.to_string())
}

/// Open settings, optionally deep-linked to a section + highlighted element.
/// Async so window creation dispatches to the main thread (a sync command would
/// deadlock: it holds the main thread while `build()` needs it).
#[tauri::command]
pub async fn open_settings(
    app: AppHandle,
    section: Option<String>,
    highlight: Option<String>,
) -> Result<(), String> {
    windows::open_settings(&app, section, highlight).map_err(|e| e.to_string())
}

/// The `route://goto` target this hub should navigate to on mount, if the
/// call that created it also routed it somewhere.
///
/// **Consumed on read.** A hub asks once at mount; leaving the target in place
/// would send the window back to Settings on every reload (and, in dev, on
/// every hot restart) long after the user had navigated away.
#[tauri::command]
pub fn get_route_target(
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
) -> Result<Option<serde_json::Value>, String> {
    Ok(state.route_targets.lock().unwrap().remove(window.label()))
}

/// Allocate a fresh label and open a brand-new chat window (Feature 5) —
/// always creates, never reuses an existing window, unlike `open_main`.
/// `handoff`, if given, is a session snapshot (the same shape the overlay's
/// Expand used to hand to `set_active_session`) stashed for the new window's
/// own one-time mount-time read via `get_pending_handoff` — keyed by this
/// window's specific label so opening several windows in a row can't race
/// each other over the same handoff, unlike the older global
/// `active_session` slot (still used, separately, by the provider
/// context-handoff gate in Settings -> Providers — not touched here).
#[tauri::command]
pub async fn open_new_chat_window(
    app: AppHandle,
    handoff: Option<serde_json::Value>,
) -> Result<(), String> {
    windows::open_new_chat_window(&app, handoff).map_err(|e| e.to_string())
}

/// One-shot read of this specific window's pending Expand handoff, if any —
/// the multi-window analog of `get_active_session`, targeted by the calling
/// window's own label (via Tauri's `Window` extractor) instead of a single
/// global slot. Removes the entry once read, so a later mount of the same
/// label (there isn't one today, since labels aren't reused, but this keeps
/// the contract "consumed exactly once" honest regardless) never re-adopts it.
#[tauri::command]
pub fn get_pending_handoff(
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
) -> Result<Option<serde_json::Value>, String> {
    Ok(state
        .pending_handoffs
        .lock()
        .unwrap()
        .remove(window.label()))
}

/// Called once by every window's frontend right after mounting. Lets the
/// dev-only load watchdog (`windows::spawn_load_watchdog`) tell a window that
/// is still loading apart from one whose first navigation failed and will
/// never load on its own — see `state::AppState::booted_windows`.
#[tauri::command]
pub fn window_ready(window: tauri::Window, state: tauri::State<'_, AppState>) {
    state
        .booted_windows
        .lock()
        .unwrap()
        .insert(window.label().to_string());
}

/// Current stack status (frontend also listens to `stack://status`).
#[tauri::command]
pub fn get_stack_status(app: tauri::AppHandle) -> Result<lifecycle::StackStatusPayload, String> {
    Ok(lifecycle::current_payload(&app))
}

/// Hotkeys that could not be registered, for a window that opened after it
/// happened.
#[tauri::command]
pub fn get_hotkey_failures(state: tauri::State<'_, AppState>) -> Result<Vec<String>, String> {
    Ok(state.hotkey_failures.lock().unwrap().clone())
}

/// What Kitty knows about the engine it is attached to, for Settings.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EngineInfo {
    /// Whether this Kitty started the engine. When another app did, the
    /// engine runs with that app's engine settings until it next restarts.
    pub spawned_by_us: bool,
    pub daemon_version: Option<String>,
    /// Whether "Restart engine" exists on this platform (not on Android,
    /// where the engine lives inside the app).
    pub can_restart: bool,
}

#[tauri::command]
pub fn get_engine_info(state: tauri::State<'_, AppState>) -> Result<EngineInfo, String> {
    let handle = state.bigtiny.lock().unwrap();
    Ok(EngineInfo {
        spawned_by_us: handle.spawned_by_us,
        daemon_version: handle.daemon_version.clone(),
        can_restart: !cfg!(target_os = "android"),
    })
}

/// Whether a load-time engine setting is waiting on a daemon restart
/// (frontend also listens to `engine://restart-state`). Lets a settings
/// window that opened after the change primed its own chip.
#[tauri::command]
pub fn get_engine_restart_state(
    app: tauri::AppHandle,
) -> Result<crate::lifecycle::engine_restart::EngineRestartState, String> {
    Ok(crate::lifecycle::engine_restart::current(&app))
}

/// One-time startup progress (frontend also listens to `stack://startup-phase`).
/// Lets a window that attaches after `start_stack` began (e.g. a slow overlay
/// mount) prime its initial phase instead of assuming `SpawningGoosed`.
#[tauri::command]
pub fn get_startup_phase(state: tauri::State<'_, AppState>) -> Result<StartupPhase, String> {
    Ok(*state.startup_phase.lock().unwrap())
}

/// Restart the engine so changed engine settings take effect.
///
/// Asks the daemon to exit, waits for it, and attaches again (which starts a
/// fresh one with the current settings). The daemon refuses while another
/// app is attached or any turn is running, and the answer names them; the
/// same state goes out on `engine://restart-state` for the banner. `force`
/// ("Restart anyway") overrides only the attached-apps check. See
/// `lifecycle::engine_restart`.
///
/// Android: the engine lives inside the app and cannot be restarted on its
/// own; engine settings apply the next time Kitty starts.
#[tauri::command]
pub async fn restart_backend(
    app: AppHandle,
    force: Option<bool>,
) -> Result<lifecycle::engine_restart::RestartOutcome, String> {
    #[cfg(target_os = "android")]
    {
        let _ = (&app, force);
        Err("Engine settings apply the next time Kitty starts.".to_string())
    }
    #[cfg(not(target_os = "android"))]
    {
        lifecycle::engine_restart::restart_now(&app, force.unwrap_or(false)).await
    }
}
