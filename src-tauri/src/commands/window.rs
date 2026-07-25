//! Window-lifecycle and backend-process commands: overlay/settings/main show,
//! stack status, and the BigTiny restart used by "Fix this" + provider switches.

use tauri::{AppHandle, Manager};

use crate::lifecycle;
use crate::state::AppState;
use crate::state::{StackStatus, StartupPhase};
use crate::windows;

/// Show/hide the overlay from the frontend.
#[tauri::command]
pub fn toggle_overlay(app: AppHandle) -> Result<(), String> {
    windows::toggle_overlay(&app).map_err(|e| e.to_string())
}

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

/// The settings deep-link target the window should navigate to on open.
#[tauri::command]
pub fn get_settings_target(
    state: tauri::State<'_, AppState>,
) -> Result<Option<serde_json::Value>, String> {
    Ok(state.settings_target.lock().unwrap().clone())
}

/// Open the full window. Async so window creation dispatches to the main thread.
#[tauri::command]
pub async fn open_main(app: AppHandle) -> Result<(), String> {
    windows::open_main(&app).map_err(|e| e.to_string())
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
    Ok(state.pending_handoffs.lock().unwrap().remove(window.label()))
}

/// Current stack status (frontend also listens to `stack://status`).
#[tauri::command]
pub fn get_stack_status(state: tauri::State<'_, AppState>) -> Result<StackStatus, String> {
    Ok(*state.stack_status.lock().unwrap())
}

/// One-time startup progress (frontend also listens to `stack://startup-phase`).
/// Lets a window that attaches after `start_stack` began (e.g. a slow overlay
/// mount) prime its initial phase instead of assuming `SpawningGoosed`.
#[tauri::command]
pub fn get_startup_phase(state: tauri::State<'_, AppState>) -> Result<StartupPhase, String> {
    Ok(*state.startup_phase.lock().unwrap())
}

/// Restart the BigTiny daemon (kills our owned process, respawns, re-syncs
/// the active provider registration). "Fix this" and the degraded-state
/// panel call this; `activate_provider` used to call it after switching, but
/// BigTiny switches providers live over REST instead.
#[tauri::command]
pub async fn restart_backend(app: AppHandle) -> Result<(), String> {
    {
        let state = app.state::<AppState>();
        state.bigtiny.lock().unwrap().process.kill_if_owned();
    }
    let (command, args, dir) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        (
            cfg.bigtiny_command.clone(),
            cfg.bigtiny_args.clone(),
            cfg.bigtiny_dir.clone(),
        )
    };
    let handle = lifecycle::bigtiny_proc::spawn(&command, &args, dir.as_deref()).await?;
    {
        let state = app.state::<AppState>();
        *state.bigtiny.lock().unwrap() = handle;
    }
    if let Err(e) = crate::bigtiny::providers::sync_active_provider(&app).await {
        tracing::warn!("bigtiny provider sync after restart failed: {e}");
    }
    crate::bigtiny::mcp::ensure_builtin_servers(&app).await;
    Ok(())
}
