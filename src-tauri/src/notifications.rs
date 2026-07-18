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

/// True if either chat surface (overlay or main) is currently visible — the
/// user can already see the response, so a toast would be redundant. Checking
/// only the overlay was a bug: with main open (and the overlay hidden, per
/// the Round-3 item 28 mutual-exclusivity rule) a toast fired anyway.
fn chat_window_visible(app: &AppHandle) -> bool {
    let visible = |label: &str| {
        app.get_webview_window(label)
            .and_then(|w| w.is_visible().ok())
            .unwrap_or(false)
    };
    visible(windows::OVERLAY) || visible(windows::MAIN)
}

/// Send a notification if no chat window is visible and the event is enabled.
/// Clicking the notification opens the main window (Round-3 item 27) — built
/// via `notify-rust` directly rather than `tauri_plugin_notification`'s
/// `.show()`, which discards the toast's activation handle and gives us no way
/// to detect a click at all.
pub fn notify_if_hidden(app: &AppHandle, event: Event, title: &str, body: &str) {
    if chat_window_visible(app) {
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

    match n.show() {
        Ok(handle) => {
            let app2 = app.clone();
            std::thread::spawn(move || {
                let _ = handle.wait_for_response(
                    move |response: &notify_rust::NotificationResponse| {
                        if response.is_default_action() {
                            let app3 = app2.clone();
                            let _ = app2.run_on_main_thread(move || {
                                let _ = windows::open_main(&app3);
                            });
                        }
                    },
                );
            });
        }
        Err(e) => tracing::warn!("notification failed: {e}"),
    }
}

/// Reflect a pending approval / running task in the tray tooltip.
pub fn set_tray_pending(app: &AppHandle, pending: bool) {
    if let Some(tray) = app.tray_by_id("main-tray") {
        let tip = if pending {
            "Goose Overlay — approval needed"
        } else {
            "Goose Overlay"
        };
        let _ = tray.set_tooltip(Some(tip));
    }
}
