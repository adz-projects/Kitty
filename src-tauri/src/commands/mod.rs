//! `#[tauri::command]` handlers — thin wrappers over the modules above. Every
//! command returns `Result<T, String>` with user-safe messages; details are
//! logged with `tracing`, not surfaced to the webview.

use tauri::{AppHandle, Manager};

use crate::config::{self, Config};
use crate::lifecycle::{self, StackStatus};
use crate::state::AppState;
use crate::{hotkey, windows};

/// Read the current app config.
#[tauri::command]
pub fn get_config(state: tauri::State<'_, AppState>) -> Result<Config, String> {
    Ok(state.config.lock().unwrap().clone())
}

/// Replace + persist the app config. Re-registers the hotkey if it changed.
#[tauri::command]
pub fn set_config(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    config: Config,
) -> Result<(), String> {
    let hotkey_changed = {
        let mut cur = state.config.lock().unwrap();
        let changed = cur.hotkey != config.hotkey;
        *cur = config.clone();
        changed
    };

    config::save(&config).map_err(|e| {
        tracing::error!("failed to save config: {e}");
        "Could not save settings to disk.".to_string()
    })?;

    if hotkey_changed {
        if let Err(e) = hotkey::register(&app, &config.hotkey) {
            tracing::error!("re-register hotkey failed: {e}");
            return Err("Saved, but the new hotkey could not be registered.".into());
        }
    }
    Ok(())
}

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

/// Open settings, optionally deep-linked to a section (Phase 5 wires targets).
#[tauri::command]
pub fn open_settings(app: AppHandle, section: Option<String>) -> Result<(), String> {
    windows::open_settings(&app, section).map_err(|e| e.to_string())
}

/// Open the full window.
#[tauri::command]
pub fn open_main(app: AppHandle) -> Result<(), String> {
    windows::open_main(&app).map_err(|e| e.to_string())
}

/// Current stack status (frontend also listens to `stack://status`).
#[tauri::command]
pub fn get_stack_status(state: tauri::State<'_, AppState>) -> Result<StackStatus, String> {
    Ok(*state.stack_status.lock().unwrap())
}

/// Restart the goosed child (kills our owned process, respawns). "Fix this" and
/// the degraded-state panel call this.
#[tauri::command]
pub async fn restart_goosed(app: AppHandle) -> Result<(), String> {
    {
        let state = app.state::<AppState>();
        state.goosed.lock().unwrap().process.kill_if_owned();
    }
    let handle = lifecycle::goosed::spawn().await?;
    let state = app.state::<AppState>();
    *state.goosed.lock().unwrap() = handle;
    Ok(())
}
