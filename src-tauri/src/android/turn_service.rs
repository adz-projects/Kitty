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
    let Ok(h) = handle() else { return };
    if let Err(e) = h.run_mobile_plugin::<serde_json::Value>("startTurnNotice", ()) {
        tracing::debug!("turn foreground service start failed: {e}");
    }
}

pub fn stop() {
    let Ok(h) = handle() else { return };
    if let Err(e) = h.run_mobile_plugin::<serde_json::Value>("stopTurnNotice", ()) {
        tracing::debug!("could not stop the turn foreground service: {e}");
    }
}
