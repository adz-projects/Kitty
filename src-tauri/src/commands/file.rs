//! File I/O and path-inspection commands: ChatML export writes, text/binary
//! attachment reads, background-image reads, and dropped-path metadata.

use std::path::PathBuf;

use serde::Serialize;
use tauri::AppHandle;

/// Read an image file as a base64 data URL (for the background image, avoiding
/// asset-protocol scope config).
#[tauri::command]
pub fn read_image_data_url(path: String) -> Result<String, String> {
    use base64::Engine;
    let bytes = std::fs::read(&path).map_err(|e| format!("could not read image: {e}"))?;
    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png")
        .to_ascii_lowercase();
    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/png",
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{mime};base64,{b64}"))
}

/// Write a UTF-8 text file (Phase 11 ChatML export). The path comes from the
/// user's native save dialog.
#[tauri::command]
pub fn write_file(path: String, content: String) -> Result<(), String> {
    std::fs::write(&path, content).map_err(|e| format!("could not write {path}: {e}"))
}

/// Read a text file for inlining into a chat-only message (Phase 9). Rejects
/// binaries and files over the cap (default 200 KB).
#[tauri::command]
pub fn read_text_file(path: String, max_bytes: Option<usize>) -> Result<String, String> {
    let cap = max_bytes.unwrap_or(200 * 1024);
    let meta = std::fs::metadata(&path).map_err(|e| format!("could not open file: {e}"))?;
    if meta.len() as usize > cap {
        return Err(format!("File is too large to attach (> {} KB).", cap / 1024));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("could not read file: {e}"))?;
    String::from_utf8(bytes)
        .map_err(|_| "That looks like a binary file — only text can be attached here.".to_string())
}

/// A file attached to a chat, classified as UTF-8 text or binary (Round-2 item 13).
#[derive(Debug, Clone, Serialize)]
pub struct FileAttachment {
    pub name: String,
    /// `"text"` or `"binary"`.
    pub kind: String,
    /// Text content for `text`; a `data:<mime>;base64,…` URL for `binary`.
    pub content: String,
    pub mime: Option<String>,
}

/// Read a dropped file for attachment to ANY provider (Round-2 item 13): UTF-8
/// files come back as text; anything else as a base64 data URL. Binaries are no
/// longer rejected. Capped (default 25 MB — large enough for a typical photo)
/// so we don't inline huge payloads.
#[tauri::command]
pub fn read_file_any(path: String, max_bytes: Option<usize>) -> Result<FileAttachment, String> {
    let cap = max_bytes.unwrap_or(25 * 1024 * 1024);
    let name = std::path::Path::new(&path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&path)
        .to_string();
    let meta = std::fs::metadata(&path).map_err(|e| format!("could not open file: {e}"))?;
    if meta.len() as usize > cap {
        return Err(format!("File is too large to attach (> {} KB).", cap / 1024));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("could not read file: {e}"))?;
    match String::from_utf8(bytes) {
        Ok(text) => Ok(FileAttachment {
            name,
            kind: "text".into(),
            content: text,
            mime: Some("text/plain".into()),
        }),
        Err(e) => {
            use base64::Engine;
            let bytes = e.into_bytes();
            let ext = std::path::Path::new(&path)
                .extension()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let mime = match ext.as_str() {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "webp" => "image/webp",
                "pdf" => "application/pdf",
                _ => "application/octet-stream",
            };
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            Ok(FileAttachment {
                name,
                kind: "binary".into(),
                content: format!("data:{mime};base64,{b64}"),
                mime: Some(mime.to_string()),
            })
        }
    }
}

/// Metadata about a dropped path (file vs. folder) for composer chips.
#[derive(Debug, Clone, Serialize)]
pub struct PathInfo {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub exists: bool,
}

/// Inspect dropped paths so the composer can show file/folder chips.
#[tauri::command]
pub fn inspect_paths(paths: Vec<String>) -> Result<Vec<PathInfo>, String> {
    Ok(paths
        .into_iter()
        .map(|p| {
            let path = PathBuf::from(&p);
            let meta = std::fs::metadata(&path);
            PathInfo {
                name: path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| p.clone()),
                is_dir: meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
                exists: meta.is_ok(),
                path: p,
            }
        })
        .collect())
}

/// Open a file/folder with the OS default handler (artifacts "Open").
#[tauri::command]
pub fn open_path(app: AppHandle, path: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_path(&path, None::<&str>)
        .map_err(|e| format!("could not open {path}: {e}"))
}

/// Reveal a file in its containing folder (artifacts "Show in Folder").
#[tauri::command]
pub fn reveal_path(app: AppHandle, path: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .reveal_item_in_dir(&path)
        .map_err(|e| format!("could not reveal {path}: {e}"))
}
