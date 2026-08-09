//! Autostart-on-login (HKCU Run key) plus the first-run completion flag.
//!
//! This used to also detect and install Ollama. That went with managed Ollama
//! itself (docs/ANDROID.md Phase 2b) — the first-run wizard now downloads a
//! GGUF instead of running a third-party installer, which needs no detection
//! step and no UAC prompt.

#[cfg(windows)]
use std::path::PathBuf;

use tauri::{AppHandle, Manager};
#[cfg(windows)]
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
#[cfg(windows)]
use winreg::RegKey;

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const RUN_VALUE: &str = "Kitty";
/// Pre-rename value name. Windows shows the value name verbatim in Task
/// Manager → Startup and in Settings → Apps → Startup, so an install that
/// enabled autostart before the Goose Overlay → Kitty rename lists itself
/// under the old product's name. Read as a fallback and cleaned up on the
/// next write (see `autostart_enabled`/`set_autostart`).
#[cfg(windows)]
const OLD_RUN_VALUE: &str = "GooseOverlay";

// Windows-only: this is the registry Run key, and there is no autostart
// equivalent shipped on Android v1 (docs/ANDROID.md D23). `commands/setup.rs`
// gates the two commands wrapping these, and `lib.rs` their handler entries.

/// True if either the current or the pre-rename value is present, so an
/// install that enabled autostart before the rename still reads as enabled
/// instead of silently appearing off (and then getting a duplicate entry
/// written under the new name).
#[cfg(windows)]
pub fn autostart_enabled() -> bool {
    let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_READ)
    else {
        return false;
    };
    key.get_value::<String, _>(RUN_VALUE).is_ok()
        || key.get_value::<String, _>(OLD_RUN_VALUE).is_ok()
}

/// Writes (or clears) the HKCU Run entry. Always removes the pre-rename
/// value too, so enabling migrates an old entry rather than leaving both
/// listed in Task Manager → Startup, and disabling can't leave a stale one
/// behind that keeps launching the app.
#[cfg(windows)]
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey_with_flags(RUN_KEY, KEY_READ | KEY_WRITE)
        .map_err(|e| e.to_string())?;
    let _ = key.delete_value(OLD_RUN_VALUE);
    if enabled {
        let exe: PathBuf = std::env::current_exe().map_err(|e| e.to_string())?;
        key.set_value(RUN_VALUE, &format!("\"{}\"", exe.display()))
            .map_err(|e| e.to_string())?;
    } else {
        let _ = key.delete_value(RUN_VALUE);
    }
    Ok(())
}

/// True if first-run setup is complete (drives wizard-vs-overlay on launch).
pub fn setup_completed(app: &AppHandle) -> bool {
    app.state::<crate::state::AppState>()
        .config
        .lock()
        .unwrap()
        .setup_completed
}
