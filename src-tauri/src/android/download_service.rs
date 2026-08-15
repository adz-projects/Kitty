//! Bracket a model download with a `dataSync` foreground service, so it keeps
//! running (and keeps its network) while Kitty is in the background.
//!
//! The transfer itself never leaves Rust — see `commands::models::run_download`.
//! All this does is tell Android the process is doing user-visible work, which
//! is the difference between a multi-GB GGUF finishing and it being frozen a
//! few minutes after the user switches apps.
//!
//! Everything here is best-effort by design. A refused notification permission,
//! a `ForegroundServiceStartNotAllowedException`, an OEM that is stricter than
//! the platform — none of those should fail the download. They only mean the
//! transfer is now at the mercy of Doze, which is exactly where it was before
//! this existed.

use serde::{Deserialize, Serialize};

use super::handle;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NoticeArgs<'a> {
    title: &'a str,
    received: u64,
    total: u64,
}

#[derive(Deserialize)]
struct Empty {}

#[derive(Deserialize)]
struct Granted {
    #[serde(default)]
    granted: bool,
}

/// Ask for POST_NOTIFICATIONS. Returns whether it ended up granted; a refusal
/// is a normal answer, and the service still runs — the user just will not see
/// its progress.
///
/// Called before the first download rather than at startup: a permission
/// prompt on first launch, before the user has asked for anything that needs
/// it, is the pattern everyone has learned to dismiss.
pub fn request_notification_permission() -> bool {
    let Ok(h) = handle() else { return false };
    match h.run_mobile_plugin::<Granted>("requestNotificationPermission", ()) {
        Ok(g) => g.granted,
        Err(e) => {
            tracing::debug!("notification permission request failed: {e}");
            false
        }
    }
}

/// Start the service, or update the notification of a running one — the same
/// call does both, since `startForegroundService` on a started service just
/// delivers another `onStartCommand`.
///
/// `total` of 0 means "unknown length": the notification shows an
/// indeterminate bar rather than inventing a percentage.
pub fn start_or_update(title: &str, received: u64, total: u64) {
    let Ok(h) = handle() else { return };
    if let Err(e) = h.run_mobile_plugin::<Empty>(
        "startDownloadNotice",
        NoticeArgs {
            title,
            received,
            total,
        },
    ) {
        tracing::debug!("download foreground service update failed: {e}");
    }
}

pub fn stop() {
    let Ok(h) = handle() else { return };
    if let Err(e) = h.run_mobile_plugin::<Empty>("stopDownloadNotice", ()) {
        tracing::debug!("could not stop the download foreground service: {e}");
    }
}
