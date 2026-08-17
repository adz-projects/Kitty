//! App config + theming commands.

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::config::{self, Config};
#[cfg(desktop)]
use crate::hotkey;
use crate::state::AppState;

/// Read the current app config.
#[tauri::command]
pub fn get_config(state: tauri::State<'_, AppState>) -> Result<Config, String> {
    Ok(state.config.lock().unwrap().clone())
}

/// One-time notice for the corrupt-config recovery path. Returns `None` when
/// config loaded fine; a user-facing message (naming the backup copy) when
/// `config.json` failed to parse at startup and settings were reset to
/// defaults — surfaced so the frontend can tell the user their saved settings
/// weren't silently discarded (they were backed up).
#[tauri::command]
pub fn get_config_recovery_notice(
    state: tauri::State<'_, AppState>,
) -> Result<Option<String>, String> {
    Ok(state.config_recovered.lock().unwrap().as_ref().map(|backup| {
        format!(
            "Kitty couldn't read its saved settings, so it reset them to defaults. \
             Your original settings were backed up to \"{backup}\" — restore anything \
             you want from there.",
        )
    }))
}

/// Replace + persist the app config. Re-registers the hotkey if it changed.
#[tauri::command]
pub async fn set_config(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    config: Config,
) -> Result<(), String> {
    // Consumed only by the desktop-only re-registration below.
    #[cfg_attr(not(desktop), allow(unused_variables))]
    let (previous, hotkey_changed, engine_changed) = {
        let mut cur = state.config.lock().unwrap();
        let hotkey_changed = cur.hotkeys != config.hotkeys
            || cur.clipboard_hotkey != config.clipboard_hotkey
            || cur.open_window_hotkey != config.open_window_hotkey;
        // Compared against the *previous* config, before it's overwritten —
        // every `[local]` knob only reaches the daemon at spawn, so a change
        // needs a restart to take effect (docs/ANDROID.md §6.4).
        let engine_changed = crate::lifecycle::engine_restart::needs_restart(&cur, &config);
        let previous = std::mem::replace(&mut *cur, config.clone());
        (previous, hotkey_changed, engine_changed)
    };

    // Both I/O chunks (disk save + hotkey re-register) run on a blocking
    // thread — off the async worker, and with the config Mutex already
    // released.
    let config_for_save = config.clone();
    let save_result = tokio::task::spawn_blocking(move || config::save(&config_for_save))
        .await
        .map_err(|e| format!("save task panicked: {e}"))?;
    if let Err(e) = save_result {
        // Roll the in-memory swap back: the app must keep running on the
        // config that's actually on disk. A failed save that leaves the two
        // diverged means the user keeps editing settings that vanish on the
        // next launch, with no error pointing at why.
        tracing::error!("failed to save config: {e}");
        *state.config.lock().unwrap() = previous;
        return Err("Could not save settings to disk.".to_string());
    }

    // Let every window re-apply theme/background from the new config.
    let _ = app.emit("theme://changed", ());

    // Restart the daemon if a load-time engine setting moved — immediately
    // when idle, queued behind an in-flight generation otherwise. Deliberately
    // after the disk save: a restart that races the save would reload the old
    // values.
    if engine_changed {
        crate::lifecycle::engine_restart::schedule(&app);
    }

    // `set_config` itself stays available on every platform — only the
    // re-registration is desktop-only, since Android has no OS-wide shortcut
    // to register (docs/ANDROID.md D23). The hotkey config fields still
    // round-trip so a config.json moves between platforms unchanged.
    #[cfg(desktop)]
    if hotkey_changed {
        let hotkeys = config.hotkeys.clone();
        let clipboard_hotkey = config.clipboard_hotkey.clone();
        let open_window_hotkey = config.open_window_hotkey.clone();
        let app2 = app.clone();
        let result = tokio::task::spawn_blocking(move || {
            hotkey::register(
                &app2,
                &hotkeys,
                clipboard_hotkey.as_deref(),
                open_window_hotkey.as_deref(),
            )
        })
        .await
        .map_err(|e| format!("hotkey re-register task panicked: {e}"))?;
        if let Err(e) = result {
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
        builtins: vec!["light".into(), "dark".into()],
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
