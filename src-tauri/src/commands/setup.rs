//! First-run wizard + Setup & Repair commands. Named `setup` (not `wizard`) to
//! avoid colliding with the top-level `crate::wizard` detection/install module
//! this file wraps.

use tauri::{AppHandle, Manager};

use crate::config;
use crate::state::AppState;
use crate::windows;
use crate::wizard;

use super::ollama::ollama_base;

/// Detect Ollama + Goose (presence, version, path).
#[tauri::command]
pub async fn detect_dependencies(app: AppHandle) -> Result<wizard::Detection, String> {
    let base = ollama_base(&app);
    Ok(wizard::detect(&base).await)
}

/// Download + launch a dependency's official installer (`ollama` / `goose`).
#[tauri::command]
pub async fn install_dependency(which: String) -> Result<(), String> {
    wizard::install(&which).await
}

/// Open the wizard in `"setup"` or `"repair"` mode.
#[tauri::command]
pub async fn open_wizard(app: AppHandle, mode: Option<String>) -> Result<(), String> {
    windows::open_wizard(&app, mode.as_deref().unwrap_or("setup")).map_err(|e| e.to_string())
}

/// The wizard launch mode the window should read on open.
#[tauri::command]
pub fn get_wizard_mode(state: tauri::State<'_, AppState>) -> Result<Option<String>, String> {
    Ok(state.wizard_mode.lock().unwrap().clone())
}

/// Mark first-run setup complete, then summon the overlay.
#[tauri::command]
pub async fn complete_setup(app: AppHandle) -> Result<(), String> {
    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        cfg.setup_completed = true;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    if let Some(win) = app.get_webview_window(windows::WIZARD) {
        let _ = win.hide();
    }
    windows::show_overlay(&app).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_autostart() -> Result<bool, String> {
    Ok(wizard::autostart_enabled())
}

#[tauri::command]
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    wizard::set_autostart(enabled)
}
