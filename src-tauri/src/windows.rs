//! Window management. One Rust process owns four labelled webview windows.
//!
//! The `overlay` is created hidden at startup and only ever shown/hidden — never
//! destroyed — because summon latency is the product (CLAUDE.md rule 1). The
//! `main`/`settings`/`wizard` windows are created lazily on first use ("hidden
//! until used") and reused thereafter.

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

pub const OVERLAY: &str = "overlay";
pub const MAIN: &str = "main";
pub const SETTINGS: &str = "settings";
/// Used by `open_wizard` once the first-run wizard is wired in Phase 7.
#[allow(dead_code)]
pub const WIZARD: &str = "wizard";

fn url(label: &str) -> WebviewUrl {
    // Path is identical in dev (vite server) and prod (dist) — see vite.config.ts.
    WebviewUrl::App(format!("src/windows/{label}/index.html").into())
}

/// Build the overlay up front, hidden. Called once from `setup`.
pub fn create_overlay(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    WebviewWindowBuilder::new(app, OVERLAY, url(OVERLAY))
        .title("Goose")
        .inner_size(760.0, 480.0)
        .min_inner_size(420.0, 240.0)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .center()
        .visible(false)
        .build()
}

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
    let win = ensure_window(app, MAIN, "Goose")?;
    win.show()?;
    win.set_focus()?;
    Ok(())
}

/// Open the settings window. `section`/`highlight` deep-linking lands in Phase 5;
/// for now they are accepted and ignored so callers can already pass them.
pub fn open_settings(app: &AppHandle, _section: Option<String>) -> tauri::Result<()> {
    let win = ensure_window(app, SETTINGS, "Goose Settings")?;
    win.show()?;
    win.set_focus()?;
    Ok(())
}

/// Open the first-run / repair wizard. Wired to the tray / degraded-state flow
/// in Phase 7; defined now so the window set is complete.
#[allow(dead_code)]
pub fn open_wizard(app: &AppHandle) -> tauri::Result<()> {
    let win = ensure_window(app, WIZARD, "Goose Setup")?;
    win.show()?;
    win.set_focus()?;
    Ok(())
}
