//! Bracket an agent turn with a `dataSync` foreground service, so the app
//! process (which on Android hosts the in-process BigTiny daemon, the loopback
//! HTTP hop, and every in-process MCP server) keeps running while Kitty is in
//! the background and the user switches to another app.
//!
//! No work happens here or in the Kotlin service — the turn runs on the daemon
//! future inside this same process. All this does is tell Android the process
//! is doing user-visible work, which is the difference between a turn finishing
//! while the user reads something else and it being frozen a few minutes after
//! they switch away.
//!
//! Modelled exactly on `download_service` — the difference is lifetime scope: a
//! turn's service is started when the SSE stream begins and stopped the moment
//! it ends (see the RAII guard in `bigtiny::stream`), rather than spanning a
//! multi-GB transfer.
//!
//! Everything here is best-effort by design. A refused notification permission,
//! a `ForegroundServiceStartNotAllowedException`, an OEM stricter than the
//! platform — none of those should fail the turn. They only mean the turn is
//! back at the mercy of Doze, which is exactly where it was before this existed.

use super::handle;

/// Start the turn foreground service (ongoing "Kitty is working…"
/// notification). Idempotent: starting an already-started service just delivers
/// another `onStartCommand`.
pub fn start() {
    ask_for_notifications_once();
    let Ok(h) = handle() else { return };
    if let Err(e) = h.run_mobile_plugin::<serde_json::Value>("startTurnNotice", ()) {
        tracing::debug!("turn foreground service start failed: {e}");
    }
}

/// Ask for POST_NOTIFICATIONS the first time a turn starts. Until now it was
/// only ever asked before a model download, so someone who never downloaded
/// one never saw a notification — including an approval a turn is waiting
/// on. The first turn is the moment it becomes useful (a turn can pause for
/// approval while the user is elsewhere). On its own thread, because the
/// request blocks until the user answers, and the turn must not wait for that.
fn ask_for_notifications_once() {
    static ASKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if ASKED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        super::download_service::request_notification_permission();
    });
}

pub fn stop() {
    let Ok(h) = handle() else { return };
    if let Err(e) = h.run_mobile_plugin::<serde_json::Value>("stopTurnNotice", ()) {
        tracing::debug!("could not stop the turn foreground service: {e}");
    }
}
