//! Global shortcut registration. Phase 0 wires the standard accelerator via
//! `tauri-plugin-global-shortcut`; Phase 6 adds the low-level Copilot-key hook.

use tauri::AppHandle;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut};

use crate::windows;

/// Register the configured accelerator (default `Alt+Space`) to toggle the
/// overlay. Any previously registered shortcut is cleared first so this can be
/// called again after the user changes the hotkey.
pub fn register(app: &AppHandle, accelerator: &str) -> Result<(), String> {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();

    let shortcut: Shortcut = accelerator
        .parse()
        .map_err(|_| format!("invalid hotkey accelerator: {accelerator}"))?;

    let handle = app.clone();
    gs.on_shortcut(shortcut, move |_app, _shortcut, event| {
        // Fire on key press only, not release, to avoid a double toggle.
        if event.state == tauri_plugin_global_shortcut::ShortcutState::Pressed {
            if let Err(e) = windows::toggle_overlay(&handle) {
                tracing::warn!("toggle_overlay from hotkey failed: {e}");
            }
        }
    })
    .map_err(|e| format!("failed to register hotkey {accelerator}: {e}"))?;

    tracing::info!("registered global hotkey: {accelerator}");
    Ok(())
}
