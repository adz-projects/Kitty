//! Writing a file out to a location the user picked, through the Storage
//! Access Framework.
//!
//! This is the only way anything leaves Kitty on Android. Every path the app
//! can write with `std::fs` is inside its own private data directory, which no
//! other app — including the system Files app — can see into. A chat export or
//! a saved artifact has to travel through a `content://` URI the user granted,
//! and `std::fs` cannot open one of those: there is no file at that location,
//! only a provider that will hand out a stream if asked through the
//! ContentResolver.
//!
//! That one mismatch broke all three save paths, each differently.
//! `write_file` ran `std::fs::write` on a URI and reported the directory as
//! unwritable; the bulk export appended `"/name.jsonl"` to a *tree* URI,
//! producing a string that is not a valid URI of either kind; and the artifact
//! download opened the document through the fs plugin with mode `"wt"`, which
//! several providers accept and then commit nothing from — a zero-byte file
//! and no error anywhere.
//!
//! One Kotlin entry point (`KittyPlugin.writeDocument`) now serves all three.
//! It takes either URI kind, creating a document inside a tree when given one,
//! and returns the byte count the provider actually accepted so the caller can
//! verify rather than trust.
//!
//! # Never call this from the main thread
//!
//! Same rule as `secrets` and `attachments`: `run_mobile_plugin` posts to the
//! Android main looper and blocks, so calling it *from* that looper deadlocks
//! the app with no error and no log line. Every caller reaches it from a
//! `spawn_blocking` worker.

use serde::{Deserialize, Serialize};

use super::handle;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WriteArgs<'a> {
    uri: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_base64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_path: Option<&'a str>,
}

/// Mirrors `KittyPlugin.writeDocument`'s resolve shape.
#[derive(Deserialize)]
pub struct WrittenDocument {
    /// The document actually written. Not necessarily the one asked for: when
    /// the target was a folder, the provider owns the final name and may
    /// de-duplicate it or adjust its extension.
    #[allow(dead_code)]
    pub uri: String,
    /// What the provider accepted, checked by `verify` against what was sent.
    pub bytes: u64,
}

/// True for the `content://` URIs both Android pickers return.
///
/// Kitty's own paths are all absolute filesystem paths, so this is the whole
/// test: anything scheme-shaped came out of a dialog and has to go through the
/// ContentResolver.
pub fn is_content_uri(path: &str) -> bool {
    path.starts_with("content://")
}

/// Write `bytes` to a picked document URI, or to `file_name` inside a picked
/// tree URI.
pub fn write_bytes(
    uri: &str,
    file_name: Option<&str>,
    mime_type: Option<&str>,
    bytes: &[u8],
) -> Result<WrittenDocument, String> {
    let encoded = crate::util::base64_encode(bytes);
    let written = handle()?
        .run_mobile_plugin::<WrittenDocument>(
            "writeDocument",
            WriteArgs {
                uri,
                file_name,
                mime_type,
                content_base64: Some(encoded),
                source_path: None,
            },
        )
        .map_err(|e| format!("could not save the file: {e}"))?;
    verify(written, bytes.len() as u64)
}

/// As `write_bytes`, streaming from a file rather than carrying its contents
/// through the JSON bridge — an artifact can be tens of megabytes, and base64
/// there would mean holding it three times over.
pub fn write_from_file(
    uri: &str,
    file_name: Option<&str>,
    mime_type: Option<&str>,
    source_path: &str,
    expected_len: u64,
) -> Result<WrittenDocument, String> {
    let written = handle()?
        .run_mobile_plugin::<WrittenDocument>(
            "writeDocument",
            WriteArgs {
                uri,
                file_name,
                mime_type,
                content_base64: None,
                source_path: Some(source_path),
            },
        )
        .map_err(|e| format!("could not save the file: {e}"))?;
    verify(written, expected_len)
}

/// A provider can accept a write and commit nothing. Saying so is the point:
/// the bug this replaces surfaced as a zero-byte file with no error anywhere,
/// which is indistinguishable from the app having saved nothing on purpose.
fn verify(written: WrittenDocument, expected: u64) -> Result<WrittenDocument, String> {
    if written.bytes != expected {
        return Err(format!(
            "the file was not saved completely — {} of {expected} bytes reached the chosen \
             location. If you picked a cloud folder, try saving to the device instead.",
            written.bytes
        ));
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_write_is_an_error_naming_both_counts() {
        let err = verify(
            WrittenDocument {
                uri: "content://x".into(),
                bytes: 0,
            },
            1234,
        )
        .unwrap_err();
        assert!(err.contains("0 of 1234"), "got: {err}");
    }

    #[test]
    fn a_complete_write_passes() {
        assert!(verify(
            WrittenDocument {
                uri: "content://x".into(),
                bytes: 99,
            },
            99
        )
        .is_ok());
    }

    #[test]
    fn only_content_uris_are_routed_through_the_resolver() {
        assert!(is_content_uri("content://com.android.providers.downloads/1"));
        assert!(!is_content_uri("/data/user/0/com.kitty.app/files/x.jsonl"));
        assert!(!is_content_uri("C:/Users/me/x.jsonl"));
    }
}
