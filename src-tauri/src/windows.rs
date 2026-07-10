//! Window management. One Rust process owns four labelled webview windows.
//!
//! The `overlay` is created hidden at startup and only ever shown/hidden — never
//! destroyed — because summon latency is the product (CLAUDE.md rule 1). The
//! `main`/`settings`/`wizard` windows are created lazily on first use ("hidden
//! until used") and reused thereafter.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::state::AppState;

/// Overlay window size (physical px). One source of truth for creation and the
/// slide-animation fallbacks (Round-5 Batch 7: height cut ~33%, from 576→386).
const OVERLAY_W: u32 = 570;
const OVERLAY_H: u32 = 386;

/// Slide-animation tuning (Round-3 follow-up: overlay rises from/sinks into the
/// taskbar rather than snapping visible/hidden).
const ANIM_STEPS: u32 = 14;
const ANIM_STEP_MS: u64 = 12;

/// Bumped at the start of every `animate_overlay_in`/`_out` call. A spawned
/// tween task checks this before each position-set step and bails out early
/// if a newer toggle has since superseded it — otherwise rapid hotkey/tray
/// spam spawns overlapping uncancelled tasks that fight over `set_position`
/// (Stage-1 close-out fix).
static ANIM_GEN: AtomicU64 = AtomicU64::new(0);

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
        .inner_size(f64::from(OVERLAY_W), f64::from(OVERLAY_H))
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

/// The overlay's resting (x, y) — lower-right of the primary monitor's *work
/// area* (which excludes the taskbar), with a small margin. Physical pixels so
/// it lands correctly regardless of DPI scaling.
#[cfg(windows)]
fn overlay_target_position(win: &WebviewWindow) -> Option<(i32, i32)> {
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
        return None;
    }
    let outer = win
        .outer_size()
        .unwrap_or(tauri::PhysicalSize::new(OVERLAY_W, OVERLAY_H));
    let margin = 12i32;
    let x = rect.right - outer.width as i32 - margin;
    let y = rect.bottom - outer.height as i32 - margin;
    Some((x, y))
}

#[cfg(not(windows))]
fn overlay_target_position(_win: &WebviewWindow) -> Option<(i32, i32)> {
    None
}

/// Position the overlay at its resting spot (used once, at creation).
fn place_overlay_bottom_right(win: &WebviewWindow) {
    if let Some((x, y)) = overlay_target_position(win) {
        let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
    }
}

/// Slide the overlay up from just below the work-area's bottom edge (as if
/// rising out of the taskbar) to its resting position, then focus it. Falls
/// back to a plain show if the work-area geometry can't be read.
fn animate_overlay_in(win: &WebviewWindow) {
    let Some((x, target_y)) = overlay_target_position(win) else {
        let _ = win.show();
        let _ = win.set_focus();
        return;
    };
    let outer = win
        .outer_size()
        .unwrap_or(tauri::PhysicalSize::new(OVERLAY_W, OVERLAY_H));
    let start_y = target_y + outer.height as i32;
    let _ = win.set_position(tauri::PhysicalPosition::new(x, start_y));
    let _ = win.show();
    let _ = win.set_focus();
    let gen = ANIM_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let win = win.clone();
    tauri::async_runtime::spawn(async move {
        for step in 1..=ANIM_STEPS {
            if ANIM_GEN.load(Ordering::SeqCst) != gen {
                return;
            }
            let t = f64::from(step) / f64::from(ANIM_STEPS);
            let y = f64::from(start_y) + (f64::from(target_y) - f64::from(start_y)) * t;
            let _ = win.set_position(tauri::PhysicalPosition::new(x, y.round() as i32));
            tokio::time::sleep(Duration::from_millis(ANIM_STEP_MS)).await;
        }
        if ANIM_GEN.load(Ordering::SeqCst) == gen {
            let _ = win.set_position(tauri::PhysicalPosition::new(x, target_y));
        }
    });
}

/// Hide the overlay immediately — no slide (owner: closing should disappear,
/// not slide; the slide-in on *show* is unaffected, still handled by
/// `animate_overlay_in`). Still bumps `ANIM_GEN` first so a concurrent
/// in-flight `animate_overlay_in` tween (e.g. a rapid re-toggle) cancels
/// itself instead of fighting this immediate hide over `set_position`.
fn animate_overlay_out(win: &WebviewWindow) {
    ANIM_GEN.fetch_add(1, Ordering::SeqCst);
    let _ = win.hide();
}

/// Show + focus the overlay, creating it if it somehow went away.
pub fn show_overlay(app: &AppHandle) -> tauri::Result<()> {
    let win = match app.get_webview_window(OVERLAY) {
        Some(w) => w,
        None => create_overlay(app)?,
    };
    animate_overlay_in(&win);
    Ok(())
}

/// Hide the overlay (kept alive for instant re-summon).
pub fn hide_overlay(app: &AppHandle) -> tauri::Result<()> {
    if let Some(win) = app.get_webview_window(OVERLAY) {
        animate_overlay_out(&win);
    }
    Ok(())
}

/// Toggle overlay visibility — the global-hotkey / tray action.
pub fn toggle_overlay(app: &AppHandle) -> tauri::Result<()> {
    match app.get_webview_window(OVERLAY) {
        Some(win) if win.is_visible().unwrap_or(false) => {
            animate_overlay_out(&win);
            Ok(())
        }
        Some(win) => {
            animate_overlay_in(&win);
            Ok(())
        }
        None => show_overlay(app),
    }
}

/// The tray-click / hotkey action (Round-3 item 28): the overlay and main
/// window are never both active at once — if main is already open, focus it
/// instead of also summoning the overlay; otherwise fall through to the usual
/// overlay toggle.
pub fn toggle_or_focus_main(app: &AppHandle) -> tauri::Result<()> {
    if let Some(win) = app.get_webview_window(MAIN) {
        if win.is_visible().unwrap_or(false) {
            win.set_focus()?;
            return Ok(());
        }
    }
    toggle_overlay(app)
}

/// Lazily create (or reuse) a normal, resizable window at the given initial
/// size (only applies on first creation — an already-open window is reused
/// as-is, matching prior behavior).
fn ensure_window(
    app: &AppHandle,
    label: &str,
    title: &str,
    initial_size: (f64, f64),
) -> tauri::Result<WebviewWindow> {
    if let Some(win) = app.get_webview_window(label) {
        return Ok(win);
    }
    WebviewWindowBuilder::new(app, label, url(label))
        .title(title)
        .inner_size(initial_size.0, initial_size.1)
        .min_inner_size(640.0, 420.0)
        .visible(false)
        .build()
}

/// Open the full window (Phase 2 binds it to the active session). 15% wider
/// than the shared settings/wizard default (Round-3 item 3).
pub fn open_main(app: &AppHandle) -> tauri::Result<()> {
    let win = ensure_window(app, MAIN, "Kitty", (1196.0, 720.0))?;
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
    let win = ensure_window(app, SETTINGS, "Kitty Settings", (1040.0, 720.0))?;
    win.show()?;
    win.set_focus()?;
    Ok(())
}

/// Open the first-run / repair wizard in the given mode (`"setup"`/`"repair"`).
/// Stores the mode (for the window's initial read) and emits it for a live nav.
pub fn open_wizard(app: &AppHandle, mode: &str) -> tauri::Result<()> {
    *app.state::<AppState>().wizard_mode.lock().unwrap() = Some(mode.to_string());
    let _ = app.emit("wizard://navigate", json!({ "mode": mode }));
    let win = ensure_window(app, WIZARD, "Kitty Setup", (1040.0, 720.0))?;
    win.show()?;
    win.set_focus()?;
    Ok(())
}
