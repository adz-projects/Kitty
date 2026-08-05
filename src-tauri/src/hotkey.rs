//! Global shortcut registration. Phase 0 wires the standard accelerator via
//! `tauri-plugin-global-shortcut`. Round-4 adds a second, distinct accelerator
//! that attaches the clipboard. (The hardware Copilot-key hook that used to
//! live alongside this was removed in the UX-simplification pass — a
//! configurable hotkey covers the same job with far less OS-level risk.)

use std::sync::{Mutex, OnceLock};

use base64::engine::general_purpose;
use base64::Engine as _;
use serde_json::json;
use tauri::{AppHandle, Emitter};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut};

use crate::windows;

/// Which handler a desired shortcut is wired to, so re-registration can leave
/// an unchanged shortcut alone and a failure can reproduce each shortcut's
/// behavior when rolling back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    ToggleMain,
    AttachClipboard,
    OpenNewWindow,
}

/// The `(shortcut, action)` pairs this process registered on its last
/// successful `register` call. Lets a later call unregister only the ones
/// being replaced — and restore them if the new batch fails — instead of
/// `unregister_all`, which would wipe *every* global shortcut on a partial
/// failure.
fn registered_track() -> &'static Mutex<Vec<(String, Action)>> {
    static TRACK: OnceLock<Mutex<Vec<(String, Action)>>> = OnceLock::new();
    TRACK.get_or_init(|| Mutex::new(Vec::new()))
}

/// Register a single parsed shortcut with its handler.
fn bind_shortcut(app: &AppHandle, accel: &str, action: Action) -> Result<(), String> {
    let shortcut: Shortcut = accel
        .parse()
        .map_err(|_| format!("invalid hotkey: {accel}"))?;
    let handle = app.clone();
    app.global_shortcut()
        .on_shortcut(shortcut, move |_app, _shortcut, event| {
            // Fire on key press only, not release, to avoid a double toggle.
            if event.state == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                match action {
                    Action::ToggleMain => {
                        if let Err(e) = windows::toggle_or_focus_main(&handle) {
                            tracing::warn!("toggle_or_focus_main from hotkey failed: {e}");
                        }
                    }
                    Action::AttachClipboard => attach_clipboard(&handle),
                    Action::OpenNewWindow => {
                        if let Err(e) = windows::open_new_chat_window(&handle, None) {
                            tracing::warn!("open_new_chat_window from hotkey failed: {e}");
                        }
                    }
                }
            }
        })
        .map_err(|e| format!("{accel}: {e}"))
}

fn unregister_shortcut(app: &AppHandle, accel: &str) {
    if let Ok(shortcut) = accel.parse::<Shortcut>() {
        let _ = app.global_shortcut().unregister(shortcut);
    }
}

/// Register every configured accelerator (default `[Alt+Space]`) to toggle the
/// overlay (Round-2 item 3), plus the optional clipboard-attach accelerator
/// (Round-4) and the open-new-chat-window accelerator (Feature 4/5).
///
/// Incremental rather than "unregister everything, then re-register": only the
/// shortcuts being replaced are unregistered, and unchanged ones are left
/// bound, so a partial failure can't leave the user with *no* working global
/// shortcuts. If anything fails, whatever this call added is dropped and the
/// previous registration set is re-applied; the error names the offenders.
pub fn register(
    app: &AppHandle,
    accelerators: &[String],
    clipboard_hotkey: Option<&str>,
    open_window_hotkey: Option<&str>,
) -> Result<(), String> {
    let mut desired: Vec<(String, Action)> = Vec::new();
    for accel in accelerators {
        desired.push((accel.clone(), Action::ToggleMain));
    }
    if let Some(accel) = clipboard_hotkey {
        desired.push((accel.to_string(), Action::AttachClipboard));
    }
    if let Some(accel) = open_window_hotkey {
        desired.push((accel.to_string(), Action::OpenNewWindow));
    }
    // Deduplicate by shortcut string (last action wins) — the same accel in
    // two groups would otherwise register twice, the second call silently
    // replacing the first's handler.
    let mut deduped: Vec<(String, Action)> = Vec::new();
    for (accel, action) in desired {
        if let Some(slot) = deduped.iter_mut().find(|(a, _)| a == &accel) {
            slot.1 = action;
        } else {
            deduped.push((accel, action));
        }
    }
    let desired = deduped;

    let mut prev = registered_track().lock().unwrap();
    let prior = prev.clone();

    // Unregister only the shortcuts that are being replaced. Anything still
    // desired is left exactly as it is — its handler is still valid.
    for (accel, _) in &prior {
        if !desired.iter().any(|(d, _)| d == accel) {
            unregister_shortcut(app, accel);
        }
    }

    let mut errors: Vec<String> = Vec::new();
    let mut newly_registered: Vec<String> = Vec::new();
    for (accel, action) in &desired {
        // Already bound by a previous call with the same behavior — no-op.
        if prior.iter().any(|(p, a)| p == accel && a == action) {
            continue;
        }
        match bind_shortcut(app, accel, *action) {
            Ok(()) => {
                tracing::info!("registered global hotkey: {accel}");
                newly_registered.push(accel.clone());
            }
            Err(e) => errors.push(e),
        }
    }

    if errors.is_empty() {
        *prev = desired;
        return Ok(());
    }

    // Partial failure — restore the prior registrations so one bad shortcut
    // can't leave the user with fewer working ones than before.
    tracing::error!(
        "global hotkey registration failed ({}); restoring previous shortcuts",
        errors.join("; ")
    );
    for accel in &newly_registered {
        unregister_shortcut(app, accel);
    }
    for (accel, action) in &prior {
        let _ = bind_shortcut(app, accel, *action);
    }
    Err(errors.join("; "))
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
