//! `lean_read_image` — reads a raster image off disk and hands it back as a
//! base64 image content block a vision model can actually see. Unlike the text
//! readers this returns image content, not a JSON text envelope, so the server
//! stub builds a `CallToolResult` from the `Ok` tuple here (see
//! `server.rs::read_image`); the daemon then injects it into the conversation
//! as an `image_url` block (`BigTinyV2/daemon/src/agent/loop_.rs`).

use crate::envelope::error_response;
use crate::paths::{path_within_allowed, resolve};
use base64::Engine;

/// Hard cap on the source image. Kept below the daemon's per-result image
/// budget (`MAX_TOOL_IMAGE_BYTES`, 4 MiB) so a read that succeeds here also
/// survives the daemon's bound rather than being silently dropped — base64
/// inflates by ~4/3, so 2.5 MiB of source stays under 4 MiB encoded.
const MAX_IMAGE_BYTES: u64 = 2_500_000;

/// Maps a file extension to an image MIME type, or `None` if it isn't an image
/// kind a vision model accepts. Keeps the tool from base64-ing an arbitrary
/// binary and calling it an image.
fn mime_for(path: &std::path::Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        Some("bmp") => Some("image/bmp"),
        _ => None,
    }
}

/// Reads `path` and returns `(base64_data, mime_type, note)` on success, or a
/// structured error envelope string on failure. `note` is a short text line the
/// caller pairs with the image block so the transcript still reads sensibly.
pub fn read_image(path: &str) -> Result<(String, String, String), String> {
    let resolved = resolve(path);
    if !path_within_allowed(&resolved) {
        return Err(error_response(
            "PATH_OUTSIDE_HOME",
            "Path is outside the directories this session may access",
            Some(&resolved.to_string_lossy()),
            Some(&crate::paths::allowed_roots_hint()),
        ));
    }
    let Some(mime) = mime_for(&resolved) else {
        return Err(error_response(
            "UNSUPPORTED_IMAGE_TYPE",
            "Not a supported image type (png, jpg, jpeg, gif, webp, bmp).",
            Some(&resolved.to_string_lossy()),
            Some("Point this tool at a raster image file; for text/PDF/Office files use the matching lean_*_read tool."),
        ));
    };
    let len = match std::fs::metadata(&resolved).map(|m| m.len()) {
        Ok(len) => len,
        Err(_) => {
            return Err(error_response(
                "FILE_NOT_FOUND",
                "Path does not exist",
                Some(&resolved.to_string_lossy()),
                None,
            ));
        }
    };
    if len > MAX_IMAGE_BYTES {
        return Err(error_response(
            "IMAGE_TOO_LARGE",
            &format!("Image is larger than the {MAX_IMAGE_BYTES} byte limit for inlining to the model."),
            Some(&resolved.to_string_lossy()),
            Some("Resize or re-save the image smaller, then read it again."),
        ));
    }
    let bytes = match std::fs::read(&resolved) {
        Ok(b) => b,
        Err(e) => {
            return Err(error_response(
                "FILE_READ_ERROR",
                &format!("Could not read the image: {e}"),
                Some(&resolved.to_string_lossy()),
                None,
            ));
        }
    };
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let name = resolved
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| resolved.to_string_lossy().into_owned());
    let note = format!("Read image {name} ({mime}, {len} bytes).");
    Ok((data, mime.to_string(), note))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_image_extension() {
        // Use a path inside the allowed home root so the boundary check (which
        // runs first) passes and we actually reach the extension check.
        let home = crate::paths::home_dir().expect("home dir");
        let path = home.join("kitty-read-image-test-notes.txt");
        let err = read_image(&path.to_string_lossy()).unwrap_err();
        assert!(err.contains("UNSUPPORTED_IMAGE_TYPE"), "{err}");
    }

    #[test]
    fn mime_mapping_covers_common_types() {
        assert_eq!(mime_for(std::path::Path::new("a.PNG")), Some("image/png"));
        assert_eq!(mime_for(std::path::Path::new("a.jpeg")), Some("image/jpeg"));
        assert_eq!(mime_for(std::path::Path::new("a.pdf")), None);
    }
}
