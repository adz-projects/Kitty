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
        return Err(format!(
            "File is too large to attach (> {} KB).",
            cap / 1024
        ));
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
        return Err(format!(
            "File is too large to attach (> {} KB).",
            cap / 1024
        ));
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

/// Copy a file into a chat session's own working directory, so the model's
/// own file tools can actually open it — real, observed bug fix: a file
/// attached in thought-partner (chat-only) mode that can't be inlined as text
/// (a `.docx`, `.pdf`, etc.) used to just get a "contents not inlined"
/// placeholder with no real path, so the model would try — and always fail —
/// to locate it on disk (outside the chat's own folder, which chat mode's
/// tool-use is scoped to). Copying the real file in means the model's own
/// document-reading tools (whatever extension provides them) can genuinely
/// open it, since it's now inside the one folder chat mode permits.
///
/// Deduplicates against an existing file of the same name in `cwd` (`report
/// (2).docx`, etc.) rather than silently overwriting — the source file is
/// untouched either way (`fs::copy`, not a move). Returns the copied file's
/// own name (not the full path — the model only ever needs the relative
/// name, since it already believes `cwd` to be "here").
#[tauri::command]
pub fn copy_file_into_chat_folder(source_path: String, cwd: String) -> Result<String, String> {
    let source = PathBuf::from(&source_path);
    let file_name = source
        .file_name()
        .ok_or_else(|| format!("'{source_path}' has no file name"))?;
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = source.extension().map(|e| e.to_string_lossy().to_string());

    let dest_dir = PathBuf::from(&cwd);
    std::fs::create_dir_all(&dest_dir)
        .map_err(|e| format!("could not create working directory {cwd}: {e}"))?;

    let mut dest = dest_dir.join(file_name);
    let mut n = 2;
    while dest.exists() {
        let candidate_name = match &ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        dest = dest_dir.join(candidate_name);
        n += 1;
    }

    std::fs::copy(&source, &dest)
        .map_err(|e| format!("could not copy {source_path} into the chat folder: {e}"))?;

    Ok(dest
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| file_name.to_string_lossy().to_string()))
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

/// A single file found in a `list_directory` scan (artifacts pane disk-scan).
#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub size: u64,
    /// Unix-epoch seconds; `0` if the platform can't report `modified`.
    pub modified: u64,
}

/// Cap on entries returned, so a huge/misused working directory can't send an
/// unbounded payload back to the frontend.
const LIST_DIRECTORY_MAX_ENTRIES: usize = 500;

/// List files (not subdirectories) directly in `path`, for the Artifacts
/// pane's disk-scan (Round-7 item 5) — surfaces files that landed in the chat
/// folder without going through a tracked tool call (e.g. dropped in via
/// Explorer). Skips hidden files (dotfiles) and directories; returns at most
/// `LIST_DIRECTORY_MAX_ENTRIES`, newest-modified first.
#[tauri::command]
pub fn list_directory(path: String) -> Result<Vec<FileEntry>, String> {
    let dir = PathBuf::from(&path);
    let entries =
        std::fs::read_dir(&dir).map_err(|e| format!("could not list directory {path}: {e}"))?;

    let mut files: Vec<FileEntry> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                return None;
            }
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            Some(FileEntry {
                name,
                path: entry.path().to_string_lossy().to_string(),
                size: meta.len(),
                modified,
            })
        })
        .collect();

    files.sort_by_key(|f| std::cmp::Reverse(f.modified));
    files.truncate(LIST_DIRECTORY_MAX_ENTRIES);
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_subdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kitty_test_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn copies_file_into_cwd_and_returns_its_name() {
        let src_dir = temp_subdir("copy_src");
        let cwd_dir = temp_subdir("copy_cwd");
        let src_file = src_dir.join("report.docx");
        std::fs::write(&src_file, b"fake docx bytes").unwrap();

        let name = copy_file_into_chat_folder(
            src_file.to_string_lossy().to_string(),
            cwd_dir.to_string_lossy().to_string(),
        )
        .unwrap();

        assert_eq!(name, "report.docx");
        assert!(cwd_dir.join("report.docx").exists());
        assert_eq!(
            std::fs::read(cwd_dir.join("report.docx")).unwrap(),
            b"fake docx bytes"
        );
        // Source untouched — a copy, not a move.
        assert!(src_file.exists());
    }

    #[test]
    fn dedupes_against_an_existing_file_of_the_same_name() {
        let src_dir = temp_subdir("dedupe_src");
        let cwd_dir = temp_subdir("dedupe_cwd");
        let src_file = src_dir.join("notes.pdf");
        std::fs::write(&src_file, b"first").unwrap();
        std::fs::write(cwd_dir.join("notes.pdf"), b"already here").unwrap();

        let name = copy_file_into_chat_folder(
            src_file.to_string_lossy().to_string(),
            cwd_dir.to_string_lossy().to_string(),
        )
        .unwrap();

        assert_eq!(name, "notes (2).pdf");
        // The pre-existing file is untouched, not overwritten.
        assert_eq!(
            std::fs::read(cwd_dir.join("notes.pdf")).unwrap(),
            b"already here"
        );
        assert_eq!(
            std::fs::read(cwd_dir.join("notes (2).pdf")).unwrap(),
            b"first"
        );
    }

    #[test]
    fn creates_the_working_directory_if_missing() {
        let src_dir = temp_subdir("mkdir_src");
        let cwd_dir = temp_subdir("mkdir_cwd").join("not_yet_created");
        let src_file = src_dir.join("a.txt");
        std::fs::write(&src_file, b"x").unwrap();

        let name = copy_file_into_chat_folder(
            src_file.to_string_lossy().to_string(),
            cwd_dir.to_string_lossy().to_string(),
        )
        .unwrap();

        assert_eq!(name, "a.txt");
        assert!(cwd_dir.join("a.txt").exists());
    }

    #[test]
    fn errors_when_the_source_file_does_not_exist() {
        let cwd_dir = temp_subdir("missing_src_cwd");
        let result = copy_file_into_chat_folder(
            "C:/definitely/does/not/exist.docx".to_string(),
            cwd_dir.to_string_lossy().to_string(),
        );
        assert!(result.is_err());
    }
}
