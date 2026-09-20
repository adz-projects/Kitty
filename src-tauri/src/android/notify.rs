//! Post a one-shot system notification from Kotlin.
//!
//! `tauri-plugin-notification` is deliberately not registered on Android — its
//! `onNewIntent` handler force-closes the app under `launchMode="singleTask"`
//! (see `lib.rs`) — so the notification path used everywhere else
//! (`notifications::emit_notification`) has no backend there. This bridges to a
//! small `@Command` on our own `kitty-native` plugin that posts a dismissable
//! notification on its own channel, tap-to-open `MainActivity`.
//!
//! Best-effort: a refused POST_NOTIFICATIONS permission or a failed round-trip
//! just means the user doesn't see a toast, which is a degradation and not an
//! error — every caller is already best-effort.

use serde::Serialize;

use super::handle;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NotifyArgs<'a> {
    title: &'a str,
    body: &'a str,
}

/// Post `title`/`body` as a system notification. No-op (logged at debug) if the
/// native plugin isn't registered or the JVM round-trip fails.
pub fn post(title: &str, body: &str) {
    let Ok(h) = handle() else { return };
    if let Err(e) =
        h.run_mobile_plugin::<serde_json::Value>("postNotification", NotifyArgs { title, body })
    {
        tracing::debug!("could not post an Android notification: {e}");
    }
}
