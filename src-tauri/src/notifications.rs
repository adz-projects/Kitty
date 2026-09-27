//! Native notifications + tray state, fired only when the relevant window is
//! hidden (CLAUDE.md Phase 3). Per-event toggles come from app config.

use tauri::{AppHandle, Manager};

use crate::config::NotificationPrefs;
use crate::state::AppState;
// Window-label/focus lookups have no meaning on Android's single always-on
// window, so the desktop focus path (and this import) is compiled out there.
#[cfg(not(target_os = "android"))]
use crate::windows;

/// Notifiable events; each maps to a per-event config toggle.
#[derive(Clone, Copy)]
pub enum Event {
    TaskComplete,
    ApprovalNeeded,
    TaskFailed,
    StackDegraded,
}

impl Event {
    /// The Android channel this event posts on (#75).
    #[cfg(target_os = "android")]
    fn channel(self) -> &'static str {
        match self {
            Event::TaskComplete => "finished",
            Event::ApprovalNeeded => "approval",
            Event::TaskFailed => "failed",
            Event::StackDegraded => "degraded",
        }
    }

    fn enabled(self, p: &NotificationPrefs) -> bool {
        match self {
            Event::TaskComplete => p.task_complete,
            Event::ApprovalNeeded => p.approval_needed,
            Event::TaskFailed => p.task_failed,
            Event::StackDegraded => p.stack_degraded,
        }
    }
}

/// True if either chat surface (overlay or main) is currently *focused* — the
/// user is actively looking at it, so a toast would be redundant. This is the
/// fallback used when a notification carries no session id (or no window is
/// currently bound to that session — see `window_focused_for_session`).
/// Deliberately checks focus, not mere visibility: a window that's open but
/// in the background (another app on top, or the user alt-tabbed away)
/// should still get a toast — that's the whole point of the feature.
#[cfg(not(target_os = "android"))]
fn chat_window_focused(app: &AppHandle) -> bool {
    let focused = |label: &str| {
        app.get_webview_window(label)
            .and_then(|w| w.is_focused().ok())
            .unwrap_or(false)
    };
    focused(windows::OVERLAY) || focused(windows::HUB)
}

/// Whether the window relevant to `session_id` (if any is currently bound to
/// one — see `windows::window_label_for_session`) is focused. Falls back to
/// `chat_window_focused`'s overlay/main check when there's no session id at
/// all (e.g. `StackDegraded`, which isn't scoped to one session) or no
/// window is currently bound to the given one (e.g. it was created headless
/// by a scheduled task with no window ever open for it).
#[cfg(not(target_os = "android"))]
fn relevant_window_focused(app: &AppHandle, session_id: Option<&str>) -> bool {
    if let Some(sid) = session_id {
        if let Some(label) = windows::window_label_for_session(app, sid) {
            return app
                .get_webview_window(&label)
                .and_then(|w| w.is_focused().ok())
                .unwrap_or(false);
        }
    }
    chat_window_focused(app)
}

/// Android has no per-window focus model — a single always-on window, with
/// `set_focus` unimplemented — so `is_focused()` can't answer "is the user
/// looking at Kitty?". The webview-driven foreground flag
/// (`AppState::foreground`, set from `visibilitychange` via
/// `set_app_foreground`) is the reliable signal instead, and it flips the
/// moment the user switches apps.
#[cfg(target_os = "android")]
fn relevant_window_focused(app: &AppHandle, _session_id: Option<&str>) -> bool {
    use std::sync::atomic::Ordering;
    app.state::<AppState>().foreground.load(Ordering::SeqCst)
}

/// Send a notification if the relevant window isn't focused and the event is
/// enabled. `session_id`, when given, targets both the focus check and the
/// click handler at the *specific* window bound to that session
/// (`windows::window_label_for_session`) rather than always the classic
/// singleton main window — falls back to the old overlay/main behavior when
/// there's no session id, or no window is currently bound to it. Built via
/// `notify-rust` directly rather than `tauri_plugin_notification`'s
/// `.show()`, which discards the toast's activation handle and gives us no
/// way to detect a click at all.
pub fn notify_if_hidden(
    app: &AppHandle,
    event: Event,
    title: &str,
    body: &str,
    session_id: Option<&str>,
) {
    if relevant_window_focused(app, session_id) {
        return;
    }
    // On Android a "stack degraded" reading while the app is backgrounded is
    // almost always the OS suspending or throttling the whole app process (the
    // daemon runs *in* that process, so the loopback health probe fails) — not
    // a real outage, and it clears the moment the app is resumed. Surfacing it
    // as a system notification every time the user switches away is pure noise,
    // and the "Open Kitty to fix it" copy is a desktop affordance anyway. The
    // in-app `stack://status` banner still updates, so a genuine problem is
    // still visible on return. Desktop keeps this notification (CLAUDE.md
    // Phase 3), where a backgrounded window doesn't suspend the backend.
    #[cfg(target_os = "android")]
    if matches!(event, Event::StackDegraded) {
        return;
    }
    let enabled = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        event.enabled(&cfg.notifications)
    };
    if !enabled {
        return;
    }

    // Android: its own channel per event, and a tap opens the chat it is
    // about (`KittyPlugin.postNotification` → `take_incoming`).
    #[cfg(target_os = "android")]
    crate::android::notify::post(event.channel(), title, body, session_id);
    #[cfg(not(target_os = "android"))]
    emit_notification(app, title, body, session_id);
}

/// Windows: `notify-rust` directly, so we keep the toast activation handle and
/// can focus the right window on click (see `notify_if_hidden`'s doc comment).
#[cfg(windows)]
fn emit_notification(app: &AppHandle, title: &str, body: &str, session_id: Option<&str>) {
    let mut n = notify_rust::Notification::new();
    n.summary(title).body(body).auto_icon();
    // Only set the AUMID for the installed app — matches
    // tauri-plugin-notification's own dev-vs-installed check, otherwise a
    // dev build (no registered shortcut) fails to show anything at all.
    if let Ok(exe) = tauri::utils::platform::current_exe() {
        if let Some(dir) = exe.parent() {
            let d = dir.display().to_string();
            if !d.ends_with("target\\debug") && !d.ends_with("target\\release") {
                n.app_id(&app.config().identifier);
            }
        }
    }

    let target_label = session_id.and_then(|sid| windows::window_label_for_session(app, sid));
    let sid_owned = session_id.map(|s| s.to_string());
    match n.show() {
        Ok(handle) => {
            // One thread per toast's response wait. `wait_for_response`
            // blocks until the toast is clicked or dismissed, and
            // notify-rust 4.x offers no timeout/try variant of it (it parks
            // on a channel `recv()`), so the previous single shared worker
            // could be stalled indefinitely by one unclicked toast — every
            // later toast's click-focus handling queued behind it and
            // starved. A parked wait now strands only its own thread; in
            // practice these threads are short-lived (Windows fires
            // Dismissed when a toast times out into the Action Center), and
            // `MAX_CLICK_TRACKER_THREADS` caps the pathological pile-up.
            let app2 = app.clone();
            let label = target_label;
            let sid = sid_owned;
            // The boxed closure captures the platform-specific handle, so the
            // concrete (not-reexported) handle type stays local to this fn.
            let wait = move || {
                let _ = handle.wait_for_response(
                    move |response: &notify_rust::NotificationResponse| {
                        if response.is_default_action() {
                            let app3 = app2.clone();
                            let label = label.clone();
                            let sid = sid.clone();
                            let _ = app2.run_on_main_thread(move || {
                                // Focus the specific window this notification
                                // was about, if one is still open.
                                let focused = label
                                    .as_deref()
                                    .map(|l| windows::show_and_focus(&app3, l))
                                    .unwrap_or(false);
                                if !focused {
                                    // No window is currently bound to this
                                    // session (e.g. the window that had it
                                    // switched to a different chat in the
                                    // meantime) — reload it into whichever
                                    // chat window is open rather than opening
                                    // a generic blank one.
                                    if let Some(sid) = sid {
                                        tauri::async_runtime::spawn(async move {
                                            windows::focus_or_open_session(&app3, &sid).await;
                                        });
                                    } else {
                                        let _ = windows::open_main(&app3);
                                    }
                                }
                            });
                        }
                    },
                );
            };
            spawn_click_tracker(wait);
        }
        Err(e) => tracing::warn!("notification failed: {e}"),
    }
}

/// Non-Windows (Android): `tauri-plugin-notification`, which is already
/// registered but was previously never called from Rust. `notify-rust` is a
/// `cfg(windows)`-only dependency and its click-to-focus machinery
/// (activation handle + blocking wait + worker thread) has no counterpart
/// here — the plugin's `show()` discards the handle, which is exactly why
/// Windows doesn't use it. So this arm posts the toast and stops there;
/// tapping it just opens the app, which is the platform norm anyway.
/// Android posts nothing for now: the plugin that would do it is not
/// registered there because its `onNewIntent` handler force-closes the app
/// (see `lib.rs`). Silent rather than an error — a missing toast is a
/// degradation, and every caller here is already best-effort.

#[cfg(all(not(windows), not(target_os = "android")))]
fn emit_notification(app: &AppHandle, title: &str, body: &str, _session_id: Option<&str>) {
    use tauri_plugin_notification::NotificationExt;
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        tracing::warn!("notification failed: {e}");
    }
}

/// Upper bound on simultaneously-live click-tracking threads. Each live
/// toast parks one thread in `wait_for_response` until the toast is answered
/// or dismissed (notify-rust has no timeout variant of that call — see the
/// note at the spawn site in `emit_notification`), so an unclicked toast
/// only ever strands its *own* thread, never a shared queue. The cap bounds
/// total thread residency when toasts pile up unanswered; past it a toast
/// still displays, it just isn't click-focusable.
#[cfg(windows)]
const MAX_CLICK_TRACKER_THREADS: usize = 16;

#[cfg(windows)]
static LIVE_CLICK_TRACKERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Run `wait` (one toast's response wait) on its own short-lived thread,
/// subject to [`MAX_CLICK_TRACKER_THREADS`]. Best-effort: when the cap is
/// reached the toast still shows — it just doesn't focus a window on click.
#[cfg(windows)]
fn spawn_click_tracker(wait: impl FnOnce() + Send + 'static) {
    use std::sync::atomic::Ordering;
    let admitted = LIVE_CLICK_TRACKERS
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < MAX_CLICK_TRACKER_THREADS).then_some(n + 1)
        })
        .is_ok();
    if !admitted {
        tracing::warn!(
            "too many unclicked toasts already awaiting a click ({MAX_CLICK_TRACKER_THREADS}); \
             this toast will show without click-to-focus"
        );
        return;
    }
    std::thread::spawn(move || {
        wait();
        LIVE_CLICK_TRACKERS.fetch_sub(1, Ordering::SeqCst);
    });
}

/// What the tray shows (#57), most urgent first: the engine is down, an
/// approval is waiting, a reply is being written, or nothing. Desktop only:
/// Android has no tray.
#[cfg_attr(not(desktop), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrayState {
    Degraded,
    Approval,
    Working,
    Idle,
}

#[cfg_attr(not(desktop), allow(dead_code))]
impl TrayState {
    /// Pure, so the precedence is testable.
    pub fn from(degraded: bool, approvals_waiting: bool, generating: bool) -> Self {
        if degraded {
            Self::Degraded
        } else if approvals_waiting {
            Self::Approval
        } else if generating {
            Self::Working
        } else {
            Self::Idle
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Self::Degraded => "Kitty — engine not running",
            Self::Approval => "Kitty — approval needed",
            Self::Working => "Kitty — working",
            Self::Idle => "Kitty",
        }
    }

    /// The badge colour drawn on the app icon, as RGBA.
    #[cfg_attr(not(desktop), allow(dead_code))]
    fn badge(self) -> Option<[u8; 4]> {
        match self {
            Self::Degraded => Some([0xE0, 0x3E, 0x3E, 0xFF]),
            Self::Approval => Some([0xF5, 0x9E, 0x0B, 0xFF]),
            Self::Working => Some([0x3B, 0x82, 0xF6, 0xFF]),
            Self::Idle => None,
        }
    }
}

/// Recompute the tray from what Kitty is doing and show it. Called wherever
/// one of the inputs changes: a turn starts or ends, an approval arrives or
/// is answered, the engine's status changes. On Android there is no tray.
pub fn refresh_tray(app: &AppHandle) {
    #[cfg(desktop)]
    {
        let state = app.state::<AppState>();
        let tray_state = TrayState::from(
            *state.stack_status.lock().unwrap() == crate::state::StackStatus::BackendDown,
            !state.pending_approvals.lock().unwrap().is_empty(),
            !state.in_flight_sessions.lock().unwrap().is_empty(),
        );
        if let Some(tray) = app.tray_by_id("main-tray") {
            let _ = tray.set_tooltip(Some(tray_state.tooltip()));
            if let Some(icon) = tray_icon(app, tray_state) {
                let _ = tray.set_icon(Some(icon));
            }
        }
    }
    #[cfg(not(desktop))]
    let _ = app;
}

/// The app icon with a coloured dot in its lower-right corner for `state`,
/// drawn once per state and kept.
#[cfg(desktop)]
fn tray_icon(app: &AppHandle, state: TrayState) -> Option<tauri::image::Image<'static>> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<TrayState, tauri::image::Image<'static>>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().unwrap().get(&state) {
        return Some(hit.clone());
    }
    let base = app.default_window_icon()?;
    let (w, h) = (base.width(), base.height());
    let mut rgba = base.rgba().to_vec();
    if let Some(color) = state.badge() {
        draw_badge(&mut rgba, w, h, color);
    }
    let image = tauri::image::Image::new_owned(rgba, w, h);
    cache.lock().unwrap().insert(state, image.clone());
    Some(image)
}

/// A filled circle with a light rim, a third of the icon wide, in the
/// lower-right corner - readable at tray size on light and dark taskbars.
#[cfg_attr(not(desktop), allow(dead_code))]
fn draw_badge(rgba: &mut [u8], w: u32, h: u32, color: [u8; 4]) {
    let r = (w.min(h) as f32) / 6.0;
    let (cx, cy) = (w as f32 - r - 1.0, h as f32 - r - 1.0);
    for y in 0..h {
        for x in 0..w {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            let px = if d <= r - 1.5 {
                color
            } else if d <= r {
                [0xFF, 0xFF, 0xFF, 0xFF]
            } else {
                continue;
            };
            let i = ((y * w + x) * 4) as usize;
            if i + 4 <= rgba.len() {
                rgba[i..i + 4].copy_from_slice(&px);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tray_shows_the_most_urgent_state() {
        assert_eq!(TrayState::from(true, true, true), TrayState::Degraded);
        assert_eq!(TrayState::from(false, true, true), TrayState::Approval);
        assert_eq!(TrayState::from(false, false, true), TrayState::Working);
        assert_eq!(TrayState::from(false, false, false), TrayState::Idle);
    }

    #[test]
    fn a_badge_is_drawn_in_the_corner_only() {
        let mut px = vec![0u8; 32 * 32 * 4];
        draw_badge(&mut px, 32, 32, [1, 2, 3, 255]);
        let at = |x: usize, y: usize| &px[(y * 32 + x) * 4..(y * 32 + x) * 4 + 4];
        assert_eq!(at(0, 0), [0, 0, 0, 0], "top-left untouched");
        assert_eq!(at(26, 26), [1, 2, 3, 255], "badge centre coloured");
    }
}
