//! Native notifications + tray state, fired only when the relevant window is
//! hidden (CLAUDE.md Phase 3). Per-event toggles come from app config.

use tauri::{AppHandle, Manager};

use crate::config::NotificationPrefs;
use crate::state::AppState;
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
    let enabled = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        event.enabled(&cfg.notifications)
    };
    if !enabled {
        return;
    }

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
#[cfg(target_os = "android")]
fn emit_notification(_app: &AppHandle, title: &str, _body: &str, _session_id: Option<&str>) {
    tracing::debug!(title, "notification suppressed: no notification backend on Android yet");
}

#[cfg(all(not(windows), not(target_os = "android")))]
fn emit_notification(app: &AppHandle, title: &str, body: &str, _session_id: Option<&str>) {
    use tauri_plugin_notification::NotificationExt;
    if let Err(e) = app
        .notification()
        .builder()
        .title(title)
        .body(body)
        .show()
    {
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

/// Reflect a pending approval / running task in the tray tooltip.
///
/// Only the *body* is gated, not the signature: `tray_by_id` is itself
/// `cfg(all(desktop, feature = "tray-icon"))` in Tauri, but this has five
/// callers across `bigtiny/stream.rs` and `commands/session/prompt.rs` that
/// shouldn't each have to know that. On Android it's a no-op — there is no
/// tray to reflect state into (docs/ANDROID.md D23/§2.5).
pub fn set_tray_pending(app: &AppHandle, pending: bool) {
    #[cfg(desktop)]
    if let Some(tray) = app.tray_by_id("main-tray") {
        let tip = if pending {
            "Kitty — approval needed"
        } else {
            "Kitty"
        };
        let _ = tray.set_tooltip(Some(tip));
    }
    #[cfg(not(desktop))]
    let _ = (app, pending);
}
