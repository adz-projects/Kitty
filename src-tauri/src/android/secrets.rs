//! Android secret storage — the replacement for `keyring` on this platform.
//!
//! `keyring` 3.6.3 has apple-native, linux-native and windows-native backends
//! and **nothing for Android**, so on `aarch64-linux-android` it falls through
//! to its catch-all in-memory mock. That compiles and appears to work, which
//! is what made D24 a silent bug: provider API keys saved fine and were gone
//! after a relaunch. `docs/ANDROID.md` planned this as "enable keyring's
//! Android backend"; there is no such backend, so this module is the store.
//!
//! The actual crypto is in Kotlin (`SecretStore.kt`) because it needs the
//! AndroidKeyStore — values are AES-256-GCM sealed under a hardware-held,
//! non-exportable key and the sealed blobs live in private SharedPreferences.
//! This file is only the boundary.
//!
//! # Never call these from the main thread
//!
//! `run_mobile_plugin` posts the call to the Android main looper and blocks
//! until it answers. Invoking it *from* that looper deadlocks the app with no
//! error and no log line. In practice that means: **a synchronous
//! `#[tauri::command]` must not touch a secret**, because Tauri runs sync
//! commands on the main thread. Every command that reads or writes one is
//! `async` for exactly this reason (see `commands::provider::delete_provider`,
//! which had to be converted), and background work reaches them from the async
//! runtime or a `spawn_blocking` worker, both of which are safe.

use serde::{Deserialize, Serialize};

use super::handle;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SecretArgs<'a> {
    account: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<&'a str>,
}

/// Mirrors `KittyPlugin.getSecret`'s resolve shape. `found` is separate from
/// `value` so that "nothing stored" is a distinct answer from "stored, and
/// here it is" — a read *failure* comes back as an `Err` instead, which is the
/// distinction `keyring::classify_read_result` exists to preserve.
#[derive(Deserialize)]
struct SecretResult {
    found: bool,
    #[serde(default)]
    value: Option<String>,
}

// The Kotlin `invoke.resolve()` (no argument) resolves the call with a JSON
// `null`, NOT `{}` — deserializing that into a struct fails with
// "invalid type: null, expected struct …", which surfaced as
// "could not store secret: failed to deserialize response: invalid type: null".
// `serde_json::Value` accepts `null` (and anything else the JVM side might send
// later), so the void commands here type their response as `Value` and ignore
// it. Do NOT swap this back to a unit struct.
pub fn set(account: &str, value: &str) -> Result<(), String> {
    handle()?
        .run_mobile_plugin::<serde_json::Value>(
            "setSecret",
            SecretArgs {
                account,
                value: Some(value),
            },
        )
        .map(|_| ())
        .map_err(|e| format!("could not store secret: {e}"))
}

/// `Ok(None)` means confirmed-absent; `Err` means the read itself failed.
/// Callers must not collapse the two — see `keyring::get_secret_checked`.
pub fn get(account: &str) -> Result<Option<String>, String> {
    let result = handle()?
        .run_mobile_plugin::<SecretResult>(
            "getSecret",
            SecretArgs {
                account,
                value: None,
            },
        )
        .map_err(|e| format!("could not read secret: {e}"))?;
    Ok(if result.found { result.value } else { None })
}

pub fn delete(account: &str) {
    let Ok(h) = handle() else { return };
    if let Err(e) = h.run_mobile_plugin::<serde_json::Value>(
        "deleteSecret",
        SecretArgs {
            account,
            value: None,
        },
    ) {
        tracing::warn!("could not delete secret for {account}: {e}");
    }
}
