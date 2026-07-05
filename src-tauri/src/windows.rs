//! Window management. One Rust process owns four labelled webview windows.
//!
//! The `overlay` is created hidden at startup and only ever shown/hidden — never
//! destroyed — because summon latency is the product (CLAUDE.md rule 1). The
//! `main`/`settings`/`wizard` windows are created lazily on first use ("hidden
//! until used") and reused thereafter.

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::state::AppState;

pub const OVERLAY: &str = "overlay";
pub const MAIN: &str = "main";
pub const SETTINGS: &str = "settings";
pub const WIZARD: &str = "wizard";

fn url(label: &str) -> WebviewUrl {
    // Path is identical in dev (vite server) and prod (dist) — see vite.config.ts.
    WebviewUrl::App(format!("src/windows/{label}/index.html").into())
}

/// Build the overlay up front, hidden. Called once from `setup`. Positioned once
/// at the lower-right of the primary monitor's work area, just above the taskbar
/// (Round-2 item 7); the user can still drag it elsewhere afterward.
pub fn create_overlay(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    let win = WebviewWindowBuilder::new(app, OVERLAY, url(OVERLAY))
        .title("Kitty")
        .inner_size(570.0, 480.0)
        .min_inner_size(360.0, 240.0)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .build()?;
    place_overlay_bottom_right(&win);
    Ok(win)
}

/// Move the overlay to the lower-right of the primary monitor's *work area*
/// (which excludes the taskbar), with a small margin. Uses physical pixels so it
/// lands correctly regardless of DPI scaling.
#[cfg(windows)]
fn place_overlay_bottom_right(win: &WebviewWindow) {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };
    let mut rect = RECT::default();
    // SAFETY: SPI_GETWORKAREA writes the primary monitor's work rect into `rect`.
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut rect as *mut _ as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if ok.is_err() {
        return;
    }
    let outer = win
        .outer_size()
        .unwrap_or(tauri::PhysicalSize::new(570, 480));
    let margin = 12i32;
    let x = rect.right - outer.width as i32 - margin;
    let y = rect.bottom - outer.height as i32 - margin;
    let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
}

#[cfg(not(windows))]
fn place_overlay_bottom_right(_win: &WebviewWindow) {}

/// Show + focus the overlay, creating it if it somehow went away.
pub fn show_overlay(app: &AppHandle) -> tauri::Result<()> {
    let win = match app.get_webview_window(OVERLAY) {
        Some(w) => w,
        None => create_overlay(app)?,
    };
    win.show()?;
    win.set_focus()?;
    Ok(())
}

/// Hide the overlay (kept alive for instant re-summon).
pub fn hide_overlay(app: &AppHandle) -> tauri::Result<()> {
    if let Some(win) = app.get_webview_window(OVERLAY) {
        win.hide()?;
    }
    Ok(())
}

/// Toggle overlay visibility — the global-hotkey / tray action.
pub fn toggle_overlay(app: &AppHandle) -> tauri::Result<()> {
    match app.get_webview_window(OVERLAY) {
        Some(win) if win.is_visible().unwrap_or(false) => win.hide(),
        Some(win) => {
            win.show()?;
            win.set_focus()
        }
        None => show_overlay(app),
    }
}

/// Lazily create (or reuse) a normal, resizable window.
fn ensure_window(app: &AppHandle, label: &str, title: &str) -> tauri::Result<WebviewWindow> {
    if let Some(win) = app.get_webview_window(label) {
        return Ok(win);
    }
    WebviewWindowBuilder::new(app, label, url(label))
        .title(title)
        .inner_size(1040.0, 720.0)
        .min_inner_size(640.0, 420.0)
        .visible(false)
        .build()
}

/// Open the full window (Phase 2 binds it to the active session).
pub fn open_main(app: &AppHandle) -> tauri::Result<()> {
    let win = ensure_window(app, MAIN, "Kitty")?;
    win.show()?;
    win.set_focus()?;
    Ok(())
}

/// Open the settings window, optionally deep-linked to a section (with an
/// element to briefly highlight). The target is stored (for the window's initial
/// read) and also emitted so an already-open window navigates.
pub fn open_settings(
    app: &AppHandle,
    section: Option<String>,
    highlight: Option<String>,
) -> tauri::Result<()> {
    if let Some(section) = section {
        let target = json!({ "section": section, "highlight": highlight });
        *app.state::<AppState>().settings_target.lock().unwrap() = Some(target.clone());
        let _ = app.emit("settings://navigate", target);
    }
    let win = ensure_window(app, SETTINGS, "Kitty Settings")?;
    win.show()?;
    win.set_focus()?;
    Ok(())
}

/// Open the first-run / repair wizard in the given mode (`"setup"`/`"repair"`).
/// Stores the mode (for the window's initial read) and emits it for a live nav.
pub fn open_wizard(app: &AppHandle, mode: &str) -> tauri::Result<()> {
    *app.state::<AppState>().wizard_mode.lock().unwrap() = Some(mode.to_string());
    let _ = app.emit("wizard://navigate", json!({ "mode": mode }));
    let win = ensure_window(app, WIZARD, "Kitty Setup")?;
    win.show()?;
    win.set_focus()?;
    Ok(())
}
