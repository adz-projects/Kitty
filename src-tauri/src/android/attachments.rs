//! Turning a picked `content://` attachment into a real file the model's tools
//! can open.
//!
//! Android's document picker (`ACTION_OPEN_DOCUMENT`, which is what Tauri's
//! dialog plugin maps to) returns a `content://` URI, not a path. Nothing in
//! Rust can open one: there is no file at that location, only a provider that
//! will hand out a stream if asked through the ContentResolver. So an attached
//! document arrived at the model as an opaque URI, and the model — correctly —
//! reported that it had no tool able to reach it.
//!
//! Deriving a display name from the URI is equally hopeless. The last path
//! segment is the provider's internal document id (`msf%3A1000000123`), which
//! is what the user saw on the attachment chip.
//!
//! Both answers live behind the ContentResolver, so both are taken in a single
//! Kotlin call (`KittyPlugin.copyContentUri`) that queries
//! `OpenableColumns.DISPLAY_NAME` and streams the bytes to a destination
//! directory. This module is only the boundary.
//!
//! # Never call this from the main thread
//!
//! Same rule as `secrets`: `run_mobile_plugin` posts to the Android main
//! looper and blocks. `commands::file::stage_attachments` reaches it from a
//! `spawn_blocking` worker, which is safe.

use serde::{Deserialize, Serialize};

use super::handle;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CopyArgs<'a> {
    uri: &'a str,
    dest_dir: &'a str,
}

/// Mirrors `KittyPlugin.copyContentUri`'s resolve shape.
#[derive(Deserialize)]
pub struct CopiedAttachment {
    /// The copy's final basename — the provider's display name, sanitized,
    /// and de-duplicated against anything already in the destination.
    pub name: String,
    /// Absolute path of the copy.
    pub path: String,
}

/// Copy a `content://` attachment into `dest_dir`.
pub fn copy_into(uri: &str, dest_dir: &str) -> Result<CopiedAttachment, String> {
    handle()?
        .run_mobile_plugin::<CopiedAttachment>("copyContentUri", CopyArgs { uri, dest_dir })
        .map_err(|e| format!("could not copy the attachment: {e}"))
}
