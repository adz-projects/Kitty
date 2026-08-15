//! Window management. One Rust process, three kinds of webview window.
//!
//! - `overlay` — created hidden at startup and only ever shown/hidden, never
//!   destroyed, because summon latency is the product (CLAUDE.md rule 1).
//! - `hub` (and `chat-N`) — the primary window, created lazily on first use
//!   and reused. It routes internally between chat, settings and setup
//!   (docs/ANDROID.md §8.1), which is why there is no longer a `settings` or
//!   `wizard` label: `open_settings`/`open_wizard` now focus a hub and emit
//!   `route://goto` at it.
//! - `screenshot-select` — a transient, transparent, always-on-top region
//!   picker sized to one monitor. Despite §8.1, this one *cannot* be a hub
//!   route: it needs its own decorationless fullscreen surface, for the same
//!   reason the overlay does.

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

fn url(label: &str) -> WebviewUrl {
    // Path is identical in dev (vite server) and prod (dist) — see vite.config.ts.
    // A dynamically-allocated `chat-N` label (D21 — multiple simultaneous hub
    // windows) has no directory of its own; every hub is functionally
    // identical, so it reuses the `hub` bundle instead.
    let dir = match label {
        OVERLAY | SCREENSHOT_SELECT => label,
        _ => HUB,
    };
    WebviewUrl::App(format!("src/windows/{dir}/index.html").into())
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
            let Ok(current) = win.url() else { continue };
            tracing::warn!(
                "window '{label}' hasn't reported ready after {attempt}x{CHECK_INTERVAL:?} \
                 (dev server hiccup?) — re-navigating to {current}"
            );
            if let Err(e) = win.navigate(current) {
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
        .min_inner_size(360.0, 240.0)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .build()?;
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
    let mut candidates: Vec<String> = vec![OVERLAY.to_string(), HUB.to_string()];
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
    let cwd = match crate::bigtiny::sessions::list(app)
        .await
        .ok()
        .and_then(|rows| find_cwd(rows, session_id))
    {
        Some(cwd) => cwd,
        None => {
            // `sessions::list` is capped at the 200 most recent sessions —
            // an older chat silently falls off that window and used to
            // resolve to `cwd: ""`. Ask once more with a much larger window
            // before settling for the default.
            crate::bigtiny::sessions::list_with_limit(app, 10_000)
                .await
                .ok()
                .and_then(|rows| find_cwd(rows, session_id))
                .unwrap_or_default()
        }
    };

    // Derive the effort control for the active provider so the re-adopted
    // session shows (or hides) the dropdown correctly, rather than the old
    // hardcoded null that always hid it.
    let thinking_effort = crate::bigtiny::effort::thinking_effort_for(app, session_id);
    let is_default_folder = crate::commands::is_default_folder(app, &cwd);
    let payload = json!({
        "session_id": session_id,
        "cwd": cwd,
        "current_mode": "approve",
        "available_modes": [],
        "thinking_effort": thinking_effort,
        "is_default_folder": is_default_folder,
    });

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
        let _ = win.set_focus();
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

/// The tray-click / hotkey action (Round-3 item 28): the overlay and main
/// window are never both active at once — if main is already open, focus it
/// instead of also summoning the overlay; otherwise fall through to the usual
/// overlay toggle.
#[cfg_attr(not(desktop), allow(dead_code))]
pub fn toggle_or_focus_main(app: &AppHandle) -> tauri::Result<()> {
    if let Some(win) = app.get_webview_window(HUB) {
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
    let win = WebviewWindowBuilder::new(app, label, url(label))
        .title(title)
        .inner_size(initial_size.0, initial_size.1)
        .min_inner_size(640.0, 420.0)
        .visible(false)
        .build()?;
    #[cfg(debug_assertions)]
    spawn_load_watchdog(app, win.clone(), label.to_string());
    Ok(win)
}

/// Open the full window (Phase 2 binds it to the active session). 15% wider
/// than the shared settings/wizard default (Round-3 item 3).
pub fn open_main(app: &AppHandle) -> tauri::Result<()> {
    let win = ensure_window(app, HUB, "Kitty", (1196.0, 720.0))?;
    win.show()?;
    win.set_focus()?;
    Ok(())
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
fn build_chat_window(app: &AppHandle, label: &str) -> tauri::Result<WebviewWindow> {
    let win = ensure_window(app, label, "Kitty", (1196.0, 720.0))?;
    win.show()?;
    win.set_focus()?;

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
    if let Err(e) = build_chat_window(app, &label) {
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
    win.set_focus()?;
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
    use super::find_cwd;
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
}
