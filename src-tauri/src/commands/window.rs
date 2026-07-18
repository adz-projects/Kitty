//! Window-lifecycle and goosed-process commands: overlay/settings/main show,
//! stack status, and the goosed restart used by "Fix this" + provider switches.

use tauri::{AppHandle, Manager};

use crate::config;
use crate::lifecycle;
use crate::state::AppState;
use crate::state::StackStatus;
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

/// Current stack status (frontend also listens to `stack://status`).
#[tauri::command]
pub fn get_stack_status(state: tauri::State<'_, AppState>) -> Result<StackStatus, String> {
    Ok(*state.stack_status.lock().unwrap())
}

/// Restart the goosed child (kills our owned process, respawns). "Fix this" and
/// the degraded-state panel call this; `activate_provider` also calls it after
/// switching so the new provider's env takes effect.
#[tauri::command]
pub async fn restart_goosed(app: AppHandle) -> Result<(), String> {
    {
        let state = app.state::<AppState>();
        state.goosed.lock().unwrap().process.kill_if_owned();
    }
    // Drop the stale ACP connection so the next session reconnects.
    {
        let state = app.state::<AppState>();
        *state.acp.lock().await = None;
    }
    let (env, goose_override) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        (
            config::providers::goosed_env(&cfg),
            cfg.goose_binary_override.clone(),
        )
    };
    let handle = lifecycle::goosed::spawn(env, goose_override.as_deref()).await?;
    let state = app.state::<AppState>();
    *state.goosed.lock().unwrap() = handle;
    Ok(())
}
