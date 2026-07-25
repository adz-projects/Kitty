//! App config + theming commands.

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::config::{self, Config};
use crate::hotkey;
use crate::state::AppState;

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
        let changed = cur.hotkeys != config.hotkeys
            || cur.clipboard_hotkey != config.clipboard_hotkey
            || cur.open_window_hotkey != config.open_window_hotkey;
        *cur = config.clone();
        changed
    };

    config::save(&config).map_err(|e| {
        tracing::error!("failed to save config: {e}");
        "Could not save settings to disk.".to_string()
    })?;

    // Let every window re-apply theme/background from the new config.
    let _ = app.emit("theme://changed", ());

    if hotkey_changed {
        if let Err(e) = hotkey::register(
            &app,
            &config.hotkeys,
            config.clipboard_hotkey.as_deref(),
            config.open_window_hotkey.as_deref(),
        ) {
            tracing::error!("re-register hotkey failed: {e}");
            return Err("Saved, but a new hotkey could not be registered.".into());
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct ThemeList {
    pub builtins: Vec<String>,
    pub user: Vec<String>,
}

/// Built-in theme names plus any user `.css` files in the themes folder.
#[tauri::command]
pub fn list_themes() -> Result<ThemeList, String> {
    let dir = config::themes_dir().map_err(|e| e.to_string())?;
    let mut user = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.to_ascii_lowercase().ends_with(".css") {
                user.push(name);
            }
        }
    }
    user.sort();
    Ok(ThemeList {
        builtins: vec!["default".into(), "dark".into()],
        user,
    })
}

/// Read a user theme's CSS text by filename (must live in the themes folder).
#[tauri::command]
pub fn read_user_theme(name: String) -> Result<String, String> {
    // Guard against path traversal — filename only.
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err("invalid theme name".into());
    }
    let path = config::themes_dir().map_err(|e| e.to_string())?.join(&name);
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("could not read theme {name}: {e}"))?;
    // Strip a leading UTF-8 BOM, which would otherwise break the first CSS rule.
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
}

/// Open the user themes folder in the file explorer.
#[tauri::command]
pub fn open_themes_folder(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let dir = config::themes_dir().map_err(|e| e.to_string())?;
    app.opener()
        .open_path(dir.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}
