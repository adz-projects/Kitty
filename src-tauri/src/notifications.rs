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
    focused(windows::OVERLAY) || focused(windows::MAIN)
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

    let mut n = notify_rust::Notification::new();
    n.summary(title).body(body).auto_icon();
    #[cfg(windows)]
    {
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
    }

    let target_label = session_id.and_then(|sid| windows::window_label_for_session(app, sid));
    let sid_owned = session_id.map(|s| s.to_string());
    match n.show() {
        Ok(handle) => {
            // One dedicated worker thread services every toast's response
            // wait (a blocking OS call). Bursts of toasts queue on it instead
            // of spawning an unbounded number of threads.
            let app2 = app.clone();
            let label = target_label;
            let sid = sid_owned;
            // The boxed closure captures the platform-specific handle, so the
            // concrete (not-reexported) handle type stays out of `ToastJob`.
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
            let job = ToastJob {
                run: Box::new(wait),
            };
            if let Err(e) = toast_worker_sender().send(job) {
                // Only fails if the worker died — the toast still shows, it
                // just isn't click-focusable.
                tracing::warn!("notification click worker unavailable: {e}");
            }
        }
        Err(e) => tracing::warn!("notification failed: {e}"),
    }
}

/// A toast whose click-detection wait still needs servicing, delivered to the
/// single notification worker (`toast_worker_sender`).
struct ToastJob {
    run: Box<dyn FnOnce() + Send>,
}

fn toast_worker_sender() -> std::sync::mpsc::Sender<ToastJob> {
    use std::sync::{Mutex, OnceLock};
    // A static rather than app state: a toast's wait can outlive the command
    // that fired it, and notifications.rs keeps no other pre-existing state.
    static WORKER: OnceLock<Mutex<Option<std::sync::mpsc::Sender<ToastJob>>>> = OnceLock::new();
    let mut guard = WORKER.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if let Some(sender) = guard.as_ref() {
        return sender.clone();
    }
    let (sender, receiver): (
        std::sync::mpsc::Sender<ToastJob>,
        std::sync::mpsc::Receiver<ToastJob>,
    ) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // One worker services every toast's response wait, sequentially.
        while let Ok(job) = receiver.recv() {
            (job.run)();
        }
    });
    *guard = Some(sender.clone());
    sender
}

/// Reflect a pending approval / running task in the tray tooltip.
pub fn set_tray_pending(app: &AppHandle, pending: bool) {
    if let Some(tray) = app.tray_by_id("main-tray") {
        let tip = if pending {
            "Kitty — approval needed"
        } else {
            "Kitty"
        };
        let _ = tray.set_tooltip(Some(tip));
    }
}
