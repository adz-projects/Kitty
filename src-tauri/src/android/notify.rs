//! Post a one-shot system notification from Kotlin.
//!
//! `tauri-plugin-notification` is deliberately not registered on Android — its
//! `onNewIntent` handler force-closes the app under `launchMode="singleTask"`
//! (see `lib.rs`) — so notifications go through a small `@Command` on our own
//! `kitty-native` plugin instead. Each kind of notice has its own channel
//! (approvals loud, the rest quieter; the user tunes them in Android's
//! Settings), and a tap opens the chat it is about (#75): the Kotlin side
//! queues the tap for `android::intents::take`.
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
    channel: &'a str,
    session_id: Option<&'a str>,
}

/// Post `title`/`body` on `channel` (`approval`, `finished`, `failed`,
/// `degraded`). No-op (logged at debug) if the native plugin isn't
/// registered or the JVM round-trip fails.
pub fn post(channel: &str, title: &str, body: &str, session_id: Option<&str>) {
    let Ok(h) = handle() else { return };
    if let Err(e) = h.run_mobile_plugin::<serde_json::Value>(
        "postNotification",
        NotifyArgs {
            title,
            body,
            channel,
            session_id,
        },
    ) {
        tracing::debug!("could not post an Android notification: {e}");
    }
}
