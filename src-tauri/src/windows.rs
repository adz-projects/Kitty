//! Window management. One Rust process, three kinds of webview window.
//!
//! - `overlay` — created hidden at startup and only ever shown/hidden, never
//!   destroyed, because summon latency is the product (CLAUDE.md rule 1).
//! - `hub` (and `chat-N`) — the primary window, created lazily on first use
//!   and reused. It routes internally between chat, settings and setup
//!   (docs/ANDROID.md §8.1), which is why there is no longer a `settings` or
//!   `wizard` label: `open_settings`/`open_wizard` now focus a hub and emit
//!   `route://goto` at it. On desktop it is decorationless and draws its own
//!   title bar, as Bacon's windows do.
//! - `screenshot-select` — a transient, transparent, always-on-top region
//!   picker sized to one monitor. Despite §8.1, this one *cannot* be a hub
//!   route: it needs its own decorationless fullscreen surface, for the same
//!   reason the overlay does.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::state::AppState;

/// Logical-pixel rectangle inside the fixed-height transparent overlay that
/// should receive pointer events (`.overlay-run-card`, open popovers, context
/// menu). Everything outside these rects passes clicks/drags straight through
/// to underlying windows.
#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
pub struct OverlayHitRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Overlay window size (logical px). Compact Windows-"Run"-style launcher bar:
/// kept at a constant transparent height (`.shadow(false)`) so upward popovers
/// and multiline prompts render inside the fixed surface without resizing or
/// moving the native `HWND` (eliminating Win32/WebView2 resize flash and shift).
const OVERLAY_W: u32 = 580;
const OVERLAY_H: u32 = 400;

/// Slide-animation tuning (Round-3 follow-up: overlay rises from/sinks into the
/// taskbar rather than snapping visible/hidden).
const ANIM_STEPS: u32 = 14;
const ANIM_STEP_MS: u64 = 12;

/// Polling cadence while the overlay is visible for toggling click-through over
/// the transparent region above the card (~60 Hz, 0% CPU while hidden).
#[cfg(windows)]
const HITTEST_POLL_MS: u64 = 16;

/// Bumped at the start of every `animate_overlay_in`/`_out` call. A spawned
/// tween task checks this before each position-set step and bails out early
/// if a newer toggle has since superseded it — otherwise rapid hotkey/tray
/// spam spawns overlapping uncancelled tasks that fight over `set_position`
/// (Stage-1 close-out fix).
static ANIM_GEN: AtomicU64 = AtomicU64::new(0);

/// Generation counter for the overlay click-through hit-test task so hiding or
/// re-showing cancels any prior polling loop cleanly.
#[cfg(windows)]
static HITTEST_GEN: AtomicU64 = AtomicU64::new(0);

/// Cached `set_ignore_cursor_events` state so Win32 window styles are only
/// touched on transitions.
#[cfg(windows)]
static OVERLAY_IGNORING_CURSOR: AtomicBool = AtomicBool::new(false);

pub const OVERLAY: &str = "overlay";

/// The primary window (docs/ANDROID.md §8.1). One window that routes between
/// chat, settings and setup, where there used to be three separate labels —
/// `main`, `settings` and `wizard`. Folding them together is what lets the
/// Android shell reuse the identical component tree behind bottom tabs
/// (§8.2) rather than growing a second UI.
///
/// Settings and setup are reached by emitting `route://goto` at a hub, not by
/// opening a window. With several hubs open at once (D21) a single shared
/// Settings window would have been ambiguous about which one's session it was
/// configuring; as a route, the answer is always "the one you clicked in".
pub const HUB: &str = "hub";
pub const SCREENSHOT_SELECT: &str = "screenshot-select";

/// `WebviewWindow::set_focus()` is unimplemented on Tauri's Android backend and
/// logs "set_focus is not yet implemented on android" every time — meaningless
/// there anyway, since Android runs a single always-focused fullscreen window.
/// This behaves exactly like `set_focus()` on desktop and is a silent no-op on
/// Android, so the hub path (`build_chat_window`/`route_to`/`open_main`) stops
/// spamming that warning.
fn focus_window(win: &WebviewWindow) -> tauri::Result<()> {
    #[cfg(not(target_os = "android"))]
    {
        win.set_focus()
    }
    #[cfg(target_os = "android")]
    {
        let _ = win;
        Ok(())
    }
}

fn url(label: &str) -> WebviewUrl {
    WebviewUrl::App(page_path(label).into())
}

/// A window's page, relative to the app root. Identical in dev (vite server)
/// and prod (dist) — see vite.config.ts. A dynamically-allocated `chat-N`
/// label (D21 — multiple simultaneous hub windows) has no directory of its
/// own; every hub is functionally identical, so it reuses the `hub` bundle.
fn page_path(label: &str) -> String {
    let dir = match label {
        OVERLAY | SCREENSHOT_SELECT => label,
        _ => HUB,
    };
    format!("src/windows/{dir}/index.html")
}

/// Dev-only watchdog for a window's first navigation. In dev the webview loads
/// over HTTP from the Vite dev server; if that navigation ever errors or
/// stalls, nothing else in this file would retry it — the overlay in
/// particular is created once at startup and only ever shown/hidden, never
/// destroyed — so the window would stay blank for the rest of the process's
/// life. Poll for the frontend's own `window_ready` ack (state.rs's
/// `booted_windows`) and re-navigate if it hasn't landed within a few
/// seconds. Compiled out of release builds: there the assets are bundled
/// locally and there is nothing transient to retry.
#[cfg(debug_assertions)]
fn spawn_load_watchdog(app: &AppHandle, win: WebviewWindow, label: String) {
    const CHECK_INTERVAL: Duration = Duration::from_secs(4);
    const MAX_ATTEMPTS: u32 = 3;
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        for attempt in 1..=MAX_ATTEMPTS {
            tokio::time::sleep(CHECK_INTERVAL).await;
            let booted = app
                .state::<AppState>()
                .booted_windows
                .lock()
                .unwrap()
                .contains(&label);
            if booted {
                return;
            }
            // The window's own page on the dev server, not whatever it shows
            // now: a first navigation that never started leaves the window on
            // `about:blank`, and reloading that would keep it blank for good.
            let page = app
                .config()
                .build
                .dev_url
                .as_ref()
                .and_then(|base| base.join(&page_path(&label)).ok());
            let Some(target) = page.or_else(|| win.url().ok()) else {
                continue;
            };
            tracing::warn!(
                "window '{label}' hasn't reported ready after {attempt}x{CHECK_INTERVAL:?} \
                 (dev server hiccup?) — re-navigating to {target}"
            );
            if let Err(e) = win.navigate(target) {
                tracing::warn!("watchdog re-navigate for '{label}' failed: {e}");
            }
        }
        tracing::warn!("window '{label}' still not ready after {MAX_ATTEMPTS} retries, giving up");
    });
}

/// Build the overlay up front, hidden. Called once from `setup`. Positioned once
/// at the lower-right of the primary monitor's work area, just above the taskbar
/// (Round-2 item 7); the user can still drag it elsewhere afterward.
///
/// Desktop-only: a borderless, always-on-top window floating over *other apps*
/// has no Android equivalent (it would need `SYSTEM_ALERT_WINDOW`, which Tauri's
/// mobile shell doesn't expose), and `decorations`/`always_on_top`/`skip_taskbar`
/// aren't even on the mobile `WebviewWindowBuilder`. Android boots straight into
/// the hub instead — docs/ANDROID.md D9/D23, §8.2.
#[cfg(desktop)]
pub fn create_overlay(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    let win = WebviewWindowBuilder::new(app, OVERLAY, url(OVERLAY))
        .title("Kitty")
        .inner_size(f64::from(OVERLAY_W), f64::from(OVERLAY_H))
        .min_inner_size(360.0, 72.0)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .build()?;
    crate::context_menu::install(&win);
    place_overlay_bottom_right(&win);
    #[cfg(debug_assertions)]
    spawn_load_watchdog(app, win.clone(), OVERLAY.to_string());
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
#[cfg(desktop)]
fn place_overlay_bottom_right(win: &WebviewWindow) {
    if let Some((x, y)) = overlay_target_position(win) {
        let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
    }
}

/// Decide whether the transparent overlay should ignore cursor events for the
/// current pointer position.
///
/// Returns `true` only when the cursor is **inside** the overlay `HWND` bounds
/// and **outside** every reported interactive rectangle (`.overlay-run-card`,
/// open `.overlay-run-popover`, `.context-menu`). When the cursor is outside
/// the `HWND` entirely — or before rects have been reported — returns `false`
/// so entering the visible card from the bottom/sides has 0 ms latency.
#[cfg_attr(not(windows), allow(dead_code))]
fn should_ignore_overlay_cursor(
    cursor_phys: (i32, i32),
    win_pos_phys: (i32, i32),
    win_size_phys: (u32, u32),
    scale: f64,
    rects: &[OverlayHitRect],
) -> bool {
    if rects.is_empty() {
        return false;
    }
    let rel_x = cursor_phys.0 - win_pos_phys.0;
    let rel_y = cursor_phys.1 - win_pos_phys.1;
    if rel_x < 0
        || rel_y < 0
        || rel_x >= win_size_phys.0 as i32
        || rel_y >= win_size_phys.1 as i32
    {
        return false;
    }
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let lx = f64::from(rel_x) / scale;
    let ly = f64::from(rel_y) / scale;
    !rects.iter().any(|r| {
        r.width > 0.0
            && r.height > 0.0
            && lx >= r.x
            && lx <= r.x + r.width
            && ly >= r.y
            && ly <= r.y + r.height
    })
}

#[cfg(windows)]
fn cursor_position_phys() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut pt = POINT::default();
    // SAFETY: GetCursorPos writes screen coordinates into `pt`.
    unsafe { GetCursorPos(&mut pt) }.ok()?;
    Some((pt.x, pt.y))
}

#[cfg(windows)]
fn set_overlay_ignoring_cursor(win: &WebviewWindow, ignore: bool) {
    if OVERLAY_IGNORING_CURSOR.swap(ignore, Ordering::SeqCst) != ignore {
        let _ = win.set_ignore_cursor_events(ignore);
    }
}

#[cfg(windows)]
fn apply_overlay_cursor_passthrough(app: &AppHandle, win: &WebviewWindow) {
    if !win.is_visible().unwrap_or(false) {
        set_overlay_ignoring_cursor(win, false);
        return;
    }
    let (Some(cursor), Ok(pos), Ok(size)) = (
        cursor_position_phys(),
        win.inner_position(),
        win.inner_size(),
    ) else {
        return;
    };
    let scale = win.scale_factor().unwrap_or(1.0);
    let rects = app
        .state::<AppState>()
        .overlay_hit_rects
        .lock()
        .unwrap()
        .clone();
    let ignore = should_ignore_overlay_cursor(
        cursor,
        (pos.x, pos.y),
        (size.width, size.height),
        scale,
        &rects,
    );
    set_overlay_ignoring_cursor(win, ignore);
}

#[cfg(windows)]
fn start_overlay_hittest(win: &WebviewWindow) {
    let gen = HITTEST_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let win = win.clone();
    let app = win.app_handle().clone();
    set_overlay_ignoring_cursor(&win, false);
    tauri::async_runtime::spawn(async move {
        let mut prev_pos: Option<(i32, i32)> = None;
        loop {
            tokio::time::sleep(Duration::from_millis(HITTEST_POLL_MS)).await;
            if HITTEST_GEN.load(Ordering::SeqCst) != gen {
                return;
            }
            if !win.is_visible().unwrap_or(false) {
                set_overlay_ignoring_cursor(&win, false);
                return;
            }
            let Ok(pos) = win.inner_position() else {
                continue;
            };
            let cur_pos = (pos.x, pos.y);
            // If the window itself is moving (summon tween or user dragging
            // the card), never flip click-through on mid-move.
            if prev_pos.is_some_and(|p| p != cur_pos) {
                prev_pos = Some(cur_pos);
                set_overlay_ignoring_cursor(&win, false);
                continue;
            }
            prev_pos = Some(cur_pos);
            apply_overlay_cursor_passthrough(&app, &win);
        }
    });
}

#[cfg(not(windows))]
fn start_overlay_hittest(_win: &WebviewWindow) {}

#[cfg(windows)]
fn stop_overlay_hittest(win: &WebviewWindow) {
    HITTEST_GEN.fetch_add(1, Ordering::SeqCst);
    set_overlay_ignoring_cursor(win, false);
}

#[cfg(not(windows))]
fn stop_overlay_hittest(_win: &WebviewWindow) {}

/// Store the overlay's current interactive logical-pixel rects and immediately
/// re-evaluate click-through state.
pub fn set_overlay_hit_rects(app: &AppHandle, rects: Vec<OverlayHitRect>) {
    {
        let state = app.state::<AppState>();
        *state.overlay_hit_rects.lock().unwrap() = rects;
    }
    #[cfg(windows)]
    if let Some(win) = app.get_webview_window(OVERLAY) {
        apply_overlay_cursor_passthrough(app, &win);
    }
}

/// Slide the overlay up from just below the work-area's bottom edge (as if
/// rising out of the taskbar) to its resting position, then focus it. Falls
/// back to a plain show if the work-area geometry can't be read.
fn animate_overlay_in(win: &WebviewWindow) {
    let target = overlay_target_position(win);
    let Some((x, target_y)) = target else {
        let _ = win.show();
        let _ = win.set_focus();
        let _ = win.emit("overlay://shown", ());
        start_overlay_hittest(win);
        return;
    };
    let scale = win.scale_factor().unwrap_or(1.0);
    let slide_px = (120.0 * scale).round() as i32;
    let start_y = target_y + slide_px;
    let _ = win.set_position(tauri::PhysicalPosition::new(x, start_y));
    let _ = win.show();
    let _ = win.set_focus();
    let _ = win.emit("overlay://shown", ());
    start_overlay_hittest(win);
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
            if let Some((rx, ry)) = overlay_target_position(&win) {
                let _ = win.set_position(tauri::PhysicalPosition::new(rx, ry));
            }
        }
    });
}

/// Hide the overlay immediately — no slide (owner: closing should disappear,
/// not slide; the slide-in on *show* is unaffected, still handled by
/// `animate_overlay_in`). Still bumps `ANIM_GEN` first so a concurrent
/// in-flight `animate_overlay_in` tween (e.g. a rapid re-toggle) cancels
/// itself instead of fighting this immediate hide over `set_position`.
fn animate_overlay_out(win: &WebviewWindow) {
    stop_overlay_hittest(win);
    ANIM_GEN.fetch_add(1, Ordering::SeqCst);
    let _ = win.hide();
}

/// Show + focus the overlay, creating it if it somehow went away.
///
/// Desktop-only (see `create_overlay`). On Android the overlay concept does
/// not exist, so this is a no-op rather than an error — callers like
/// `complete_setup` shouldn't have to branch on platform just to finish.
#[cfg(desktop)]
pub fn show_overlay(app: &AppHandle) -> tauri::Result<()> {
    let win = match app.get_webview_window(OVERLAY) {
        Some(w) => w,
        None => create_overlay(app)?,
    };
    animate_overlay_in(&win);
    Ok(())
}

#[cfg(not(desktop))]
pub fn show_overlay(_app: &AppHandle) -> tauri::Result<()> {
    Ok(())
}

/// Whether any Kitty window is on screen (visible and not minimized). When
/// none is, something that needs the user - an approval - summons the
/// overlay rather than waiting in a window nobody can see.
#[cfg(desktop)]
pub fn any_kitty_window_visible(app: &AppHandle) -> bool {
    app.webview_windows()
        .values()
        .any(|w| w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false))
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

/// Find the label of whichever open window is currently bound to
/// `session_id` (`AppState.chat_windows` — kept up to date by the frontend's
/// `bind_window_session` call whenever a window establishes or switches its
/// active session). Used to target a notification click (and the
/// visibility check that gates firing one at all) at the *specific* window
/// a session lives in, instead of a fixed singleton — see
/// `notifications.rs`.
///
/// Unused on Android: there is no per-window model there (a single always-on
/// window), so the notification path uses the app-level foreground flag instead
/// of resolving a session to a window — see `notifications::relevant_window_focused`.
#[cfg_attr(target_os = "android", allow(dead_code))]
pub fn window_label_for_session(app: &AppHandle, session_id: &str) -> Option<String> {
    let state = app.state::<AppState>();
    let map = state.chat_windows.lock().unwrap();
    map.iter()
        .find(|(_, sid)| sid.as_deref() == Some(session_id))
        .map(|(label, _)| label.clone())
}

/// Any currently open chat-capable window (overlay/main/a `chat-N`), for the
/// notification-click fallback when the target session isn't bound to any
/// specific window (`window_label_for_session` returned `None` — the window
/// that had it moved on to a different session in the meantime). Prefers a
/// focused window, then any merely-visible one. Deliberately does NOT treat
/// the overlay's mere *existence* as "open" — it's created hidden at startup
/// and lives forever (rule 1), so `app.get_webview_window(OVERLAY).is_some()`
/// is true even when the user has never summoned it this session.
// The chat-window focus/routing helpers below are reached today only from
// desktop surfaces (tray, global hotkey, single-instance, notification
// clicks). They're live code, not dead — Android simply has no caller for
// them until Phase 6b gives the hub its own routing (docs/ANDROID.md §8.2),
// so they're marked per-function rather than blanket-allowed.
#[cfg_attr(not(desktop), allow(dead_code))]
fn any_open_chat_window(app: &AppHandle) -> Option<String> {
    let mut candidates: Vec<String> = vec![HUB.to_string()];
    {
        let state = app.state::<AppState>();
        let map = state.chat_windows.lock().unwrap();
        let mut labels: Vec<String> = map.keys().cloned().collect();
        labels.sort();
        candidates.extend(labels);
    }
    let is_focused = |label: &str| {
        app.get_webview_window(label)
            .and_then(|w| w.is_focused().ok())
            .unwrap_or(false)
    };
    let is_visible = |label: &str| {
        app.get_webview_window(label)
            .and_then(|w| w.is_visible().ok())
            .unwrap_or(false)
    };
    if let Some(l) = candidates.iter().find(|l| is_focused(l)) {
        return Some(l.clone());
    }
    candidates.into_iter().find(|l| is_visible(l))
}

/// Notification-click fallback for a session no longer bound to any specific
/// window: rather than opening a generic blank window (the confirmed bug —
/// switching to a different chat in the same window, then clicking the
/// completion toast for the one just switched away from, popped a brand-new
/// blank window instead of returning to it), reload the session into
/// whichever chat-capable window is already open, or a fresh one if none is.
/// Reuses the exact same `chat://adopt-session` -> `adoptSession()` ->
/// `loadSession()` path Expand's handoff already uses, so this gets a full,
/// correct replay rather than a half-populated snapshot.
#[cfg_attr(not(desktop), allow(dead_code))]
pub async fn focus_or_open_session(app: &AppHandle, session_id: &str) {
    let payload = if let Some(handoff) =
        crate::overlay_runs::handoff_for_unhanded_run(app, session_id)
    {
        handoff
    } else {
        let cwd = match crate::bigtiny::sessions::list(app)
            .await
            .ok()
            .and_then(|rows| find_cwd(rows, session_id))
        {
            Some(cwd) => cwd,
            None => {
                // `sessions::list` is capped at the 200 most recent sessions —
                // an older chat falls off that window and used to resolve to
                // `cwd: ""`. Read that one session's row instead (the engine caps
                // a list page at 500, so asking for a longer list did not help).
                match crate::bigtiny::client::ensure_client(app) {
                    Ok(client) => crate::bigtiny::sessions::session_row(&client, session_id)
                        .await
                        .map(|row| crate::bigtiny::sessions::translate_session_row(&row))
                        .and_then(|row| find_cwd(vec![row], session_id))
                        .unwrap_or_default(),
                    Err(_) => String::new(),
                }
            }
        };

        // Derive the effort control for the active provider so the re-adopted
        // session shows (or hides) the dropdown correctly, rather than the old
        // hardcoded null that always hid it.
        let thinking_effort = crate::bigtiny::effort::thinking_effort_for(app, session_id);
        let is_default_folder = crate::commands::is_default_folder(app, &cwd);
        json!({
            "session_id": session_id,
            "cwd": cwd,
            "thinking_effort": thinking_effort,
            "is_default_folder": is_default_folder,
        })
    };

    if let Some(label) = any_open_chat_window(app) {
        let _ = app.emit_to(&label, "chat://adopt-session", payload);
        show_and_focus(app, &label);
    } else {
        let _ = open_new_chat_window(app, Some(payload));
    }
}

/// Pull the stored `cwd` for `session_id` out of a session-list payload.
/// Pure so the find-vs-miss decision (which `focus_or_open_session` retries
/// once with a larger listing window) stays unit-testable.
#[cfg_attr(not(desktop), allow(dead_code))]
fn find_cwd(rows: Vec<serde_json::Value>, session_id: &str) -> Option<String> {
    rows.into_iter()
        .find(|r| r.get("sessionId").and_then(|v| v.as_str()) == Some(session_id))
        .and_then(|r| r.get("cwd").and_then(|v| v.as_str()).map(str::to_string))
}

/// Show + focus an already-open window by label, animating the overlay in
/// if that's the target (matching its usual summon behavior rather than a
/// plain, unanimated show) — generalizes `open_main`'s show+focus to any
/// window label, including a dynamically-allocated `chat-N` one. Returns
/// `false` if no window with that label is currently open (caller decides
/// the fallback).
#[cfg_attr(not(desktop), allow(dead_code))]
pub fn show_and_focus(app: &AppHandle, label: &str) -> bool {
    let Some(win) = app.get_webview_window(label) else {
        return false;
    };
    if label == OVERLAY {
        animate_overlay_in(&win);
    } else {
        let _ = win.show();
        let _ = focus_window(&win);
    }
    true
}

/// Taskbar-icon click behavior: a second launch attempt (double-clicking the
/// exe again, or clicking its taskbar-pinned/Start-menu shortcut while
/// already running) is caught by the single-instance plugin and routed here
/// instead of spawning a new process (see `lib.rs`). Deliberately excludes
/// the overlay entirely — unlike `any_open_chat_window`'s notification-click
/// fallback, which treats the overlay as just another chat-capable surface,
/// a taskbar-icon click should always resolve to the full chat-window
/// experience (`main` or a `chat-N`): focus one if any exists (preferring an
/// already-focused one, else the first found), otherwise open a brand-new
/// one. Never creates or shows the overlay.
#[cfg_attr(not(desktop), allow(dead_code))]
pub fn focus_or_open_chat_window(app: &AppHandle) {
    let mut candidates: Vec<String> = vec![HUB.to_string()];
    {
        let state = app.state::<AppState>();
        let map = state.chat_windows.lock().unwrap();
        let mut labels: Vec<String> = map.keys().cloned().collect();
        labels.sort();
        candidates.extend(labels);
    }
    let is_focused = |label: &str| {
        app.get_webview_window(label)
            .and_then(|w| w.is_focused().ok())
            .unwrap_or(false)
    };
    let exists = |label: &str| app.get_webview_window(label).is_some();

    let target = candidates
        .iter()
        .find(|l| is_focused(l))
        .or_else(|| candidates.iter().find(|l| exists(l)));

    match target {
        Some(label) => {
            show_and_focus(app, label);
        }
        None => {
            let _ = open_new_chat_window(app, None);
        }
    }
}

/// The tray-click action (Round-3 item 28): the overlay and main
/// window are never both active at once — if main is already open, focus it
/// instead of also summoning the overlay; otherwise fall through to the usual
/// overlay toggle.
#[cfg_attr(not(desktop), allow(dead_code))]
pub fn toggle_or_focus_main(app: &AppHandle) -> tauri::Result<()> {
    // Any full chat window: the hub, or one opened by Expand / "New Chat
    // Window" (`chat-N`) - after an Expand the chat lives there, and
    // summoning the overlay instead would open an empty one (#52).
    let mut labels = vec![HUB.to_string()];
    {
        let state = app.state::<AppState>();
        let map = state.chat_windows.lock().unwrap();
        let mut chat: Vec<String> = map
            .keys()
            .filter(|l| l.as_str() != OVERLAY)
            .cloned()
            .collect();
        chat.sort();
        labels.extend(chat);
    }
    let windows: Vec<WebviewWindow> = labels
        .iter()
        .filter_map(|l| app.get_webview_window(l))
        .filter(|w| w.is_visible().unwrap_or(false))
        .collect();
    if let Some(win) = windows
        .iter()
        .find(|w| w.is_focused().unwrap_or(false))
        .or_else(|| windows.first())
    {
        focus_window(win)?;
        return Ok(());
    }
    toggle_overlay(app)
}

/// Hide every visible Kitty window, returning their labels so
/// [`restore_windows`] can bring back exactly those - for a screenshot, which
/// must not capture Kitty itself (#30).
#[cfg(desktop)]
pub fn hide_all_windows(app: &AppHandle) -> Vec<String> {
    let mut hidden = Vec::new();
    for (label, win) in app.webview_windows() {
        if label != SCREENSHOT_SELECT && win.is_visible().unwrap_or(false) {
            let _ = win.hide();
            hidden.push(label);
        }
    }
    hidden
}

#[cfg(desktop)]
pub fn restore_windows(app: &AppHandle, labels: &[String]) {
    for label in labels {
        if let Some(win) = app.get_webview_window(label) {
            let _ = win.show();
            if label == OVERLAY {
                start_overlay_hittest(&win);
                let _ = focus_window(&win);
                let _ = win.emit("overlay://shown", ());
            }
        }
    }
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
    let builder = WebviewWindowBuilder::new(app, label, url(label))
        .title(title)
        .inner_size(initial_size.0, initial_size.1)
        .min_inner_size(640.0, 420.0)
        .visible(false);
    // The hub draws its own title bar (`components/shared/TitleBar.tsx`), the
    // same construction as Bacon's windows, so the desktop window has no
    // native frame. Windows keeps the drop shadow and the resize edges.
    #[cfg(desktop)]
    let builder = builder.decorations(false);
    let win = builder.build()?;
    crate::context_menu::install(&win);
    #[cfg(debug_assertions)]
    spawn_load_watchdog(app, win.clone(), label.to_string());
    Ok(win)
}

/// Open the full window (Phase 2 binds it to the active session). 15% wider
/// than the shared settings/wizard default (Round-3 item 3).
pub fn open_main(app: &AppHandle) -> tauri::Result<()> {
    let win = ensure_window(app, HUB, "Kitty", (1196.0, 720.0))?;
    win.show()?;
    focus_window(&win)?;
    Ok(())
}

/// Consider the user "actively typing/interacting" if OS input occurred within
/// this window, unless that input was the submit keystroke itself.
#[cfg_attr(not(desktop), allow(dead_code))]
const ACTIVE_INPUT_GRACE_MS: u64 = 1500;

/// Milliseconds since the last keyboard or mouse input event in Windows.
#[cfg(windows)]
pub fn user_idle_ms() -> Option<u64> {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut lii = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: `cbSize` matches `LASTINPUTINFO` as required by Win32.
    let ok = unsafe { GetLastInputInfo(&mut lii) }.as_bool();
    if !ok {
        return None;
    }
    // SAFETY: `GetTickCount` takes no pointers; `wrapping_sub` handles 49.7d rollover.
    let now_ms = unsafe { GetTickCount() };
    Some(u64::from(now_ms.wrapping_sub(lii.dwTime)))
}

#[cfg(not(windows))]
#[cfg_attr(not(desktop), allow(dead_code))]
pub fn user_idle_ms() -> Option<u64> {
    None
}

/// Decide whether revealing a background-run chat window should steal
/// foreground focus or open quietly in the background (`SW_SHOWNOACTIVATE` +
/// taskbar flash).
///
/// Always focuses when `force_focus` is true (tray / notification click), when
/// OS idle time cannot be read, when the user has been idle for >= 1.5s, or
/// when the last OS input happened around submission (the user just hit Enter
/// and is waiting for Kitty). Defers focus when the user interacted again
/// more than 300ms after submitting — they switched to another app and are
/// actively typing or clicking there.
#[cfg_attr(not(desktop), allow(dead_code))]
pub fn should_steal_focus_on_reveal(
    force_focus: bool,
    elapsed_since_submit_ms: u64,
    idle_ms: Option<u64>,
) -> bool {
    if force_focus {
        return true;
    }
    let Some(idle) = idle_ms else {
        return true;
    };
    if idle >= ACTIVE_INPUT_GRACE_MS {
        return true;
    }
    elapsed_since_submit_ms <= idle.saturating_add(300)
}

#[cfg(windows)]
fn reveal_window_without_focus(win: &WebviewWindow) -> tauri::Result<()> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        FlashWindowEx, ShowWindow, FLASHWINFO, FLASHW_TIMERNOFG, FLASHW_TRAY, SW_SHOWNOACTIVATE,
    };
    let raw_hwnd = win.hwnd()?;
    let hwnd = HWND(raw_hwnd.0 as _);
    let fwi = FLASHWINFO {
        cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
        hwnd,
        dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
        uCount: 3,
        dwTimeout: 0,
    };
    // SAFETY: `hwnd` belongs to `win` and `fwi.cbSize` is initialized to `FLASHWINFO`'s size.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = FlashWindowEx(&fwi);
    }
    Ok(())
}

#[cfg(not(windows))]
#[cfg_attr(not(desktop), allow(dead_code))]
fn reveal_window_without_focus(win: &WebviewWindow) -> tauri::Result<()> {
    win.show()
}

/// Open a brand-new chat window under a freshly-allocated label (Feature 5 —
/// multiple simultaneous chat windows, each on a different conversation).
/// Unlike `open_main`, this is only ever called with a label the caller just
/// allocated and has never used before, so `ensure_window`'s get-or-create
/// always takes its "create" branch here — there is no reuse-by-label case
/// to consider. Registers a cleanup hook so `AppState.chat_windows`/
/// `pending_handoffs` don't leak an entry once the window closes
/// (bookkeeping only — the underlying BigTiny session is untouched and
/// remains resumable from history).
fn build_chat_window(
    app: &AppHandle,
    label: &str,
    steal_focus: bool,
) -> tauri::Result<WebviewWindow> {
    let win = ensure_window(app, label, "Kitty", (1196.0, 720.0))?;
    if steal_focus {
        win.show()?;
        focus_window(&win)?;
    } else {
        reveal_window_without_focus(&win)?;
    }

    let cleanup_app = app.clone();
    let cleanup_label = label.to_string();
    win.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Destroyed) {
            let state = cleanup_app.state::<AppState>();
            state.chat_windows.lock().unwrap().remove(&cleanup_label);
            state
                .pending_handoffs
                .lock()
                .unwrap()
                .remove(&cleanup_label);
        }
    });

    Ok(win)
}

/// Create (or reuse, if somehow still around from an aborted prior capture)
/// the screenshot region-selection window, sized and positioned via
/// *physical* pixels to exactly cover the full virtual-desktop rect
/// (`screenshot::virtual_screen_rect`) — spanning every monitor regardless
/// of per-monitor DPI, so the selection window's own fractional click
/// coordinates (its CSS-pixel width/height against the window's real
/// on-screen extent) translate back to physical screen coordinates without
/// needing any `devicePixelRatio` arithmetic on the frontend side. Hidden
/// until the caller has stashed the preview in `AppState.screenshot_preview`
/// (avoids a blank-then-populated flash).
///
/// Async and awaited by its only caller (`capture_screenshot_region`): a
/// stale window from an aborted capture is destroyed and the teardown is
/// awaited *before* rebuilding under the same label, so the rebuild never
/// races the old window's destruction in the window manager.
/// Windows-only, like the Win32 GDI capture it serves (`crate::screenshot`);
/// its only caller, `commands::screenshot`, is gated the same way.
#[cfg(windows)]
pub async fn create_screenshot_select_window(
    app: &AppHandle,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> tauri::Result<WebviewWindow> {
    if let Some(win) = app.get_webview_window(SCREENSHOT_SELECT) {
        let _ = win.destroy();
        wait_for_window_gone(app, SCREENSHOT_SELECT).await;
    }
    let win = WebviewWindowBuilder::new(app, SCREENSHOT_SELECT, url(SCREENSHOT_SELECT))
        .title("Kitty — Select a region")
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .build()?;
    win.set_position(tauri::PhysicalPosition::new(x, y))?;
    win.set_size(tauri::PhysicalSize::new(
        width.max(1) as u32,
        height.max(1) as u32,
    ))?;
    Ok(win)
}

/// Poll until `label` is no longer registered in the window manager, bounded
/// so a stubborn window can't hang the caller. After `destroy()` the native
/// teardown is dispatched to the main thread; this gives it a moment to land
/// before the caller rebuilds a window with the same label.
#[cfg(windows)]
async fn wait_for_window_gone(app: &AppHandle, label: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if app.get_webview_window(label).is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tracing::warn!("window '{label}' did not tear down within 5s; proceeding anyway");
}

/// Allocate a fresh label and open a brand-new chat window — always creates,
/// never reuses an existing window (unlike `open_main`/`toggle_or_focus_main`).
/// `handoff`, if given, is a session snapshot (the shape the overlay's Expand
/// hands over) stashed under the new window's own label for its one-time
/// mount-time read via `get_pending_handoff`. Plain sync function so it can
/// be called both from the async `commands::open_new_chat_window` Tauri
/// command (IPC from the frontend) and directly from the tray's sync
/// menu-event handler (already on the main thread, same as `open_main`'s own
/// direct-call pattern there) without needing two separate implementations.
pub fn open_new_chat_window(
    app: &AppHandle,
    handoff: Option<serde_json::Value>,
) -> tauri::Result<()> {
    open_new_chat_window_with_focus(app, handoff, true)
}

/// Like [`open_new_chat_window`], except `steal_focus = false` reveals the
/// window quietly (`SW_SHOWNOACTIVATE` + taskbar flash on Windows) instead of
/// yanking keyboard focus away from another app.
pub fn open_new_chat_window_with_focus(
    app: &AppHandle,
    handoff: Option<serde_json::Value>,
    steal_focus: bool,
) -> tauri::Result<()> {
    let label = {
        let state = app.state::<AppState>();
        let mut counter = state.next_chat_window_id.lock().unwrap();
        *counter += 1;
        let label = format!("chat-{}", *counter);
        state
            .chat_windows
            .lock()
            .unwrap()
            .insert(label.clone(), None);
        if let Some(payload) = handoff {
            state
                .pending_handoffs
                .lock()
                .unwrap()
                .insert(label.clone(), payload);
        }
        label
    };
    if let Err(e) = build_chat_window(app, &label, steal_focus) {
        // A failed build must not leak the label (and its handoff slot) into
        // the bookkeeping maps — `any_open_chat_window`/`focus_or_open_chat_window`
        // would otherwise route to a window that doesn't exist for the rest
        // of the process's life.
        let state = app.state::<AppState>();
        state.chat_windows.lock().unwrap().remove(&label);
        state.pending_handoffs.lock().unwrap().remove(&label);
        return Err(e);
    }
    Ok(())
}

/// Focus a hub window (opening one if none exists) and route it to `view`.
///
/// The target is both stored and emitted, and both are needed: a hub that
/// already exists navigates on the event, while one created by this call
/// isn't listening yet and reads the stored target once at mount
/// (`get_route_target`). Storing it under the specific label rather than
/// globally is what keeps D21's multiple hubs from all jumping to Settings
/// when one of them is asked to.
fn route_to(app: &AppHandle, target: serde_json::Value) -> tauri::Result<()> {
    let label = existing_chat_window(app).unwrap_or_else(|| HUB.to_string());
    {
        let state = app.state::<AppState>();
        state
            .route_targets
            .lock()
            .unwrap()
            .insert(label.clone(), target.clone());
    }
    // Addressed to the one window, not broadcast: `emit_to` is the difference
    // between "open Settings here" and "open Settings everywhere".
    let _ = app.emit_to(label.as_str(), "route://goto", target);
    let win = ensure_window(app, &label, "Kitty", (1196.0, 720.0))?;
    win.show()?;
    focus_window(&win)?;
    Ok(())
}

/// A hub-capable window that already exists, preferring a focused one. `None`
/// when only the overlay is around, in which case the caller creates `HUB`.
fn existing_chat_window(app: &AppHandle) -> Option<String> {
    let mut candidates: Vec<String> = vec![HUB.to_string()];
    {
        let state = app.state::<AppState>();
        let map = state.chat_windows.lock().unwrap();
        let mut labels: Vec<String> = map.keys().cloned().collect();
        labels.sort();
        candidates.extend(labels);
    }
    let alive: Vec<String> = candidates
        .into_iter()
        .filter(|l| app.get_webview_window(l).is_some())
        .collect();
    alive
        .iter()
        .find(|l| {
            app.get_webview_window(l)
                .and_then(|w| w.is_focused().ok())
                .unwrap_or(false)
        })
        .cloned()
        .or_else(|| alive.first().cloned())
}

/// Open Settings, optionally deep-linked to a section (with an element to
/// briefly highlight). A route within a hub, not a window of its own — see
/// [`HUB`].
pub fn open_settings(
    app: &AppHandle,
    section: Option<String>,
    highlight: Option<String>,
) -> tauri::Result<()> {
    route_to(
        app,
        json!({ "view": "settings", "section": section, "highlight": highlight }),
    )
}

/// Open the first-run / repair flow in the given mode (`"setup"`/`"repair"`).
pub fn open_wizard(app: &AppHandle, mode: &str) -> tauri::Result<()> {
    route_to(app, json!({ "view": "wizard", "mode": mode }))
}

#[cfg(test)]
mod tests {
    use super::{
        find_cwd, should_ignore_overlay_cursor, should_steal_focus_on_reveal, OverlayHitRect,
    };
    use serde_json::json;

    #[test]
    fn find_cwd_matches_the_session_and_reads_its_cwd() {
        let rows = vec![
            json!({"sessionId": "a", "cwd": "C:/x"}),
            json!({"sessionId": "b", "cwd": "C:/y"}),
        ];
        assert_eq!(find_cwd(rows, "b"), Some("C:/y".to_string()));
    }

    #[test]
    fn find_cwd_returns_none_when_the_session_is_outside_the_window() {
        // The miss case `focus_or_open_session` retries with a larger limit.
        let rows = vec![json!({"sessionId": "a", "cwd": "C:/x"})];
        assert_eq!(find_cwd(rows, "older-session"), None);
    }

    #[test]
    fn find_cwd_returns_none_when_the_row_has_no_cwd() {
        let rows = vec![json!({"sessionId": "a"})];
        assert_eq!(find_cwd(rows, "a"), None);
    }

    #[test]
    fn overlay_cursor_passes_through_only_in_transparent_region_above_card() {
        let card = OverlayHitRect {
            x: 0.0,
            y: 306.0,
            width: 580.0,
            height: 94.0,
        };
        let popover = OverlayHitRect {
            x: 140.0,
            y: 120.0,
            width: 240.0,
            height: 180.0,
        };
        // Window at physical (1000, 400), logical 580x400 at 1.5x DPI -> 870x600 physical.
        let win_pos = (1000, 400);
        let win_size = (870, 600);
        let scale = 1.5;

        // Fail open when no rects have been reported yet.
        assert!(!should_ignore_overlay_cursor((1100, 500), win_pos, win_size, scale, &[]));

        // Outside the HWND completely -> keep armed (false) for 0ms entry latency.
        assert!(!should_ignore_overlay_cursor((500, 500), win_pos, win_size, scale, &[card]));

        // Inside transparent region above the card (logical y = 200 < 306) -> pass through!
        assert!(should_ignore_overlay_cursor((1200, 700), win_pos, win_size, scale, &[card]));

        // Inside the visible card (logical y = 350 in [306, 400]) -> interactive!
        assert!(!should_ignore_overlay_cursor((1200, 925), win_pos, win_size, scale, &[card]));

        // With an open popover: cursor over popover (logical 200, 200) -> interactive!
        assert!(!should_ignore_overlay_cursor(
            (1300, 700),
            win_pos,
            win_size,
            scale,
            &[card, popover]
        ));
        // To the right of the open popover (logical 480, 200) -> still pass through!
        assert!(should_ignore_overlay_cursor(
            (1720, 700),
            win_pos,
            win_size,
            scale,
            &[card, popover]
        ));
    }

    #[test]
    fn focus_safe_reveal_respects_active_typing_after_submit() {
        // Explicit open / notification click always steals focus.
        assert!(should_steal_focus_on_reveal(true, 5000, Some(50)));
        // Missing idle reading falls back to focusing.
        assert!(should_steal_focus_on_reveal(false, 5000, None));
        // User idle >= 1.5s focuses immediately.
        assert!(should_steal_focus_on_reveal(false, 5000, Some(1500)));
        // Fast reply before user touched anything else (< 300ms gap) focuses.
        assert!(should_steal_focus_on_reveal(false, 800, Some(750)));
        // User switched apps and typed again after submitting -> do not steal focus!
        assert!(!should_steal_focus_on_reveal(false, 4000, Some(120)));
    }
}
