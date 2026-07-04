//! Native notifications + tray state, fired only when the relevant window is
//! hidden (CLAUDE.md Phase 3). Per-event toggles come from app config.

use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

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

/// True if the overlay is currently visible (so we can skip notifying).
fn overlay_visible(app: &AppHandle) -> bool {
    app.get_webview_window(windows::OVERLAY)
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false)
}

/// Send a notification if the overlay is hidden and the event is enabled.
pub fn notify_if_hidden(app: &AppHandle, event: Event, title: &str, body: &str) {
    if overlay_visible(app) {
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
