//! Global shortcut registration. Phase 0 wires the standard accelerator via
//! `tauri-plugin-global-shortcut`; Phase 6 adds the low-level Copilot-key hook.

use tauri::AppHandle;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut};

use crate::windows;

/// Register every configured accelerator (default `[Alt+Space]`) to toggle the
/// overlay (Round-2 item 3). Previous registrations are cleared first so this can
/// be called again after the user edits the list. Invalid/failed accelerators are
/// skipped and collected into the returned error, so the good ones still bind.
pub fn register(app: &AppHandle, accelerators: &[String]) -> Result<(), String> {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();

    let mut errors: Vec<String> = Vec::new();
    for accel in accelerators {
        let shortcut: Shortcut = match accel.parse() {
            Ok(s) => s,
            Err(_) => {
                errors.push(format!("invalid hotkey: {accel}"));
                continue;
            }
        };
        let handle = app.clone();
        match gs.on_shortcut(shortcut, move |_app, _shortcut, event| {
            // Fire on key press only, not release, to avoid a double toggle.
            if event.state == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                if let Err(e) = windows::toggle_overlay(&handle) {
                    tracing::warn!("toggle_overlay from hotkey failed: {e}");
                }
            }
        }) {
            Ok(()) => tracing::info!("registered global hotkey: {accel}"),
            Err(e) => errors.push(format!("{accel}: {e}")),
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}
