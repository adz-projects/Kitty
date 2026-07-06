//! Global shortcut registration. Phase 0 wires the standard accelerator via
//! `tauri-plugin-global-shortcut`; Phase 6 adds the low-level Copilot-key hook.
//! Round-4 adds a second, distinct accelerator that attaches the clipboard.

use base64::engine::general_purpose;
use base64::Engine as _;
use serde_json::json;
use tauri::{AppHandle, Emitter};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut};

use crate::windows;

/// Register every configured accelerator (default `[Alt+Space]`) to toggle the
/// overlay (Round-2 item 3), plus the optional clipboard-attach accelerator
/// (Round-4). Previous registrations are cleared first so this can be called
/// again after the user edits either list — both must be (re-)registered in
/// the same call, since `unregister_all` would otherwise wipe whichever one
/// isn't passed. Invalid/failed accelerators are skipped and collected into
/// the returned error, so the good ones still bind.
pub fn register(
    app: &AppHandle,
    accelerators: &[String],
    clipboard_hotkey: Option<&str>,
) -> Result<(), String> {
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
                if let Err(e) = windows::toggle_or_focus_main(&handle) {
                    tracing::warn!("toggle_or_focus_main from hotkey failed: {e}");
                }
            }
        }) {
            Ok(()) => tracing::info!("registered global hotkey: {accel}"),
            Err(e) => errors.push(format!("{accel}: {e}")),
        }
    }

    if let Some(accel) = clipboard_hotkey {
        match accel.parse::<Shortcut>() {
            Ok(shortcut) => {
                let handle = app.clone();
                match gs.on_shortcut(shortcut, move |_app, _shortcut, event| {
                    if event.state == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                        attach_clipboard(&handle);
                    }
                }) {
                    Ok(()) => tracing::info!("registered clipboard hotkey: {accel}"),
                    Err(e) => errors.push(format!("{accel}: {e}")),
                }
            }
            Err(_) => errors.push(format!("invalid clipboard hotkey: {accel}")),
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Summon the overlay with the current clipboard pre-attached (Round-4
/// clipboard hotkey + tray "Ask about clipboard" item): image takes priority
/// over text (arboard only ever surfaces one or the other in practice).
/// Clipboard reads happen off the main thread per the plugin's own guidance;
/// `show_overlay` (a window op) is then dispatched back onto it.
pub fn attach_clipboard(app: &AppHandle) {
    let app_bg = app.clone();
    std::thread::spawn(move || {
        let clipboard = app_bg.clipboard();
        let payload = clipboard
            .read_image()
            .ok()
            .and_then(encode_clipboard_image)
            .map(|data_url| json!({ "kind": "image", "mime": "image/png", "data_url": data_url }))
            .or_else(|| {
                clipboard
                    .read_text()
                    .ok()
                    .filter(|t| !t.trim().is_empty())
                    .map(|text| json!({ "kind": "text", "text": text }))
            });

        let app_main = app_bg.clone();
        let _ = app_bg.run_on_main_thread(move || {
            if let Err(e) = windows::show_overlay(&app_main) {
                tracing::warn!("clipboard-attach show_overlay failed: {e}");
            }
            if let Some(p) = payload {
                let _ = app_main.emit("clipboard://attach", p);
            }
        });
    });
}

/// Re-encode arboard's raw RGBA clipboard image as a PNG data URL, matching
/// `read_file_any`'s existing format so the frontend's image-attachment path
/// (Round-3 item 17) is reused verbatim.
fn encode_clipboard_image(image: tauri::image::Image) -> Option<String> {
    let img = image::RgbaImage::from_raw(image.width(), image.height(), image.rgba().to_vec())?;
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .ok()?;
    Some(format!(
        "data:image/png;base64,{}",
        general_purpose::STANDARD.encode(&buf)
    ))
}
