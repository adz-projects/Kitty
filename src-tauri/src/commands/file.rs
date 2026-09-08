//! File I/O and path-inspection commands: ChatML export writes, text/binary
//! attachment reads, and dropped-path metadata.

use std::path::PathBuf;

use serde::Serialize;
use tauri::AppHandle;

/// Write a UTF-8 text file (Phase 11 ChatML export). The path comes from the
/// user's native save dialog. Async + `spawn_blocking`: a sync command runs
/// on the main thread, and a large transcript write has no business
/// stalling the UI.
#[tauri::command]
pub async fn write_file(path: String, content: String) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        std::fs::write(&path, content).map_err(|e| format!("could not write {path}: {e}"))
    })
    .await
    .map_err(|e| format!("file write task panicked: {e}"))?
}

/// Read a text file for inlining into a chat-only message (Phase 9). Rejects
/// binaries and files over the cap (default 200 KB).
#[tauri::command]
pub async fn read_text_file(path: String, max_bytes: Option<usize>) -> Result<String, String> {
    tokio::task::spawn_blocking(move || {
        let cap = max_bytes.unwrap_or(200 * 1024);
        let meta = std::fs::metadata(&path).map_err(|e| format!("could not open file: {e}"))?;
        if meta.len() as usize > cap {
            return Err(format!(
                "File is too large to attach (> {} KB).",
                cap / 1024
            ));
        }
        let bytes = std::fs::read(&path).map_err(|e| format!("could not read file: {e}"))?;
        String::from_utf8(bytes).map_err(|_| {
            "That looks like a binary file — only text can be attached here.".to_string()
        })
    })
    .await
    .map_err(|e| format!("file read task panicked: {e}"))?
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
pub async fn read_file_any(
    path: String,
    max_bytes: Option<usize>,
) -> Result<FileAttachment, String> {
    tokio::task::spawn_blocking(move || {
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
        read_file_any_from_bytes(bytes, &name)
    })
    .await
    .map_err(|e| format!("file read task panicked: {e}"))?
}

/// Pure half of `read_file_any`'s classification (kept small and blocking-free
/// so the wrapping `spawn_blocking` closure stays obvious).
fn read_file_any_from_bytes(bytes: Vec<u8>, name: &str) -> Result<FileAttachment, String> {
    match String::from_utf8(bytes) {
        Ok(text) => Ok(FileAttachment {
            name: name.to_string(),
            kind: "text".into(),
            content: text,
            mime: Some("text/plain".into()),
        }),
        Err(e) => {
            use base64::Engine;
            let bytes = e.into_bytes();
            let ext = std::path::Path::new(name)
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
                name: name.to_string(),
                kind: "binary".into(),
                content: format!("data:{mime};base64,{b64}"),
                mime: Some(mime.to_string()),
            })
        }
    }
}

/// Upper bound for [`copy_file_into_chat_folder`]: big enough for any real
/// document/photo/recording someone attaches, small enough that a
/// pathological pick can't spend minutes duplicating gigabytes into the
/// chat folder while the user waits.
const COPY_INTO_CHAT_FOLDER_MAX_BYTES: u64 = 512 * 1024 * 1024;

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
///
/// Async + `spawn_blocking` (same pattern as `read_file_any`): a sync
/// command runs on the main thread, and an unbounded copy of a large
/// attachment froze the UI. Capped at [`COPY_INTO_CHAT_FOLDER_MAX_BYTES`].
#[tauri::command]
pub async fn copy_file_into_chat_folder(
    source_path: String,
    cwd: String,
) -> Result<String, String> {
    tokio::task::spawn_blocking(move || copy_file_into_chat_folder_blocking(&source_path, &cwd))
        .await
        .map_err(|e| format!("file copy task panicked: {e}"))?
}

/// Blocking half of [`copy_file_into_chat_folder`], split out so the
/// `spawn_blocking` wrapper stays trivial and the logic stays unit-testable
/// without a runtime.
fn copy_file_into_chat_folder_blocking(source_path: &str, cwd: &str) -> Result<String, String> {
    let source = PathBuf::from(source_path);
    let file_name = source
        .file_name()
        .ok_or_else(|| format!("'{source_path}' has no file name"))?;
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = source.extension().map(|e| e.to_string_lossy().to_string());

    let meta =
        std::fs::metadata(&source).map_err(|e| format!("could not open {source_path}: {e}"))?;
    if meta.len() > COPY_INTO_CHAT_FOLDER_MAX_BYTES {
        return Err(format!(
            "File is too large to attach (> {} MB).",
            COPY_INTO_CHAT_FOLDER_MAX_BYTES / (1024 * 1024)
        ));
    }

    let dest_dir = PathBuf::from(cwd);
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
/// Async + `spawn_blocking`: each entry is a disk `metadata` call, and a
/// sync command runs on the main thread.
#[tauri::command]
pub async fn inspect_paths(paths: Vec<String>) -> Result<Vec<PathInfo>, String> {
    tokio::task::spawn_blocking(move || {
        paths
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
            .collect::<Vec<PathInfo>>()
    })
    .await
    .map_err(|e| format!("path inspection task panicked: {e}"))
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

/// Save a copy of an artifact wherever the user chooses ("Download").
///
/// This exists for Android, where the chat folder lives inside the app's
/// private data directory: `open_path` / `reveal_path` are useless there
/// because no other app — including the system Files app — can see into it,
/// so a file the model wrote would otherwise be trapped. The save dialog maps
/// to `ACTION_CREATE_DOCUMENT`, which hands back a `content://` URI the user
/// picked (typically Downloads), granting write access to that one document
/// without the app holding any storage permission.
///
/// Returns `Ok(false)` when the user dismissed the dialog — a cancel is a
/// normal outcome, not an error the UI should report.
#[tauri::command]
pub async fn download_file(app: AppHandle, path: String) -> Result<bool, String> {
    use tauri_plugin_dialog::DialogExt;

    let source = PathBuf::from(&path);
    let name = source
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| format!("{path} has no file name"))?;

    // The dialog is callback-based on every platform; bridge it to async so
    // the command can await the choice and report the copy's outcome.
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_file_name(&name)
        .save_file(move |chosen| {
            let _ = tx.send(chosen);
        });
    let Some(target) = rx
        .await
        .map_err(|_| "the save dialog closed unexpectedly")?
    else {
        return Ok(false);
    };

    #[cfg(target_os = "android")]
    {
        // A `content://` URI is not a filesystem path, so it can't be
        // `fs::copy`d — the fs plugin resolves it through the Android
        // ContentResolver and hands back a real file descriptor.
        use std::io::Write;
        use tauri_plugin_fs::{FsExt, OpenOptions};

        let bytes = std::fs::read(&source).map_err(|e| format!("could not read {name}: {e}"))?;
        let mut opts = OpenOptions::new();
        opts.write(true).truncate(true);
        let mut out = app
            .fs()
            .open(target, opts.clone())
            .map_err(|e| format!("could not open the chosen location: {e}"))?;
        out.write_all(&bytes)
            .map_err(|e| format!("could not save {name}: {e}"))?;
        out.flush()
            .map_err(|e| format!("could not save {name}: {e}"))?;
    }
    #[cfg(not(target_os = "android"))]
    {
        let dest = target
            .into_path()
            .map_err(|e| format!("could not resolve the chosen location: {e}"))?;
        std::fs::copy(&source, &dest).map_err(|e| format!("could not save {name}: {e}"))?;
    }

    Ok(true)
}

/// A single file (or, as of release-fixes-2, subfolder) found in a
/// `list_directory` scan (artifacts pane disk-scan).
#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub size: u64,
    /// Unix-epoch seconds; `0` if the platform can't report `modified`.
    pub modified: u64,
    /// `true` for a direct subdirectory of the scanned folder. The Artifacts
    /// pane shows these as a distinct "open in Explorer" row, not a file
    /// card — Kitty doesn't traverse into them, just surfaces that they
    /// exist (release-fixes-2: "show subfolders in Artifacts — just a way
    /// to open them in explorer, no traversing them").
    pub is_dir: bool,
}

/// Cap on entries returned, so a huge/misused working directory can't send an
/// unbounded payload back to the frontend.
const LIST_DIRECTORY_MAX_ENTRIES: usize = 500;

/// List files and direct subdirectories in `path`, for the Artifacts pane's
/// disk-scan (Round-7 item 5; subfolders added in release-fixes-2) —
/// surfaces files (and folders) that landed in the chat folder without going
/// through a tracked tool call (e.g. dropped in via Explorer). Skips hidden
/// entries (dotfiles/dot-directories); does not recurse into
/// subdirectories — only their own name/path is reported, not their
/// contents. Returns at most `LIST_DIRECTORY_MAX_ENTRIES` combined, newest
/// files first, with subdirectories always sorted after files (`is_dir` is a
/// coarser sort key than `modified`, so subfolders don't jostle position in
/// the list as their contents change).
#[tauri::command]
pub async fn list_directory(path: String) -> Result<Vec<FileEntry>, String> {
    tokio::task::spawn_blocking(move || {
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
                if !meta.is_file() && !meta.is_dir() {
                    return None; // symlinks/special files — not modeled here
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
                    is_dir: meta.is_dir(),
                })
            })
            .collect();

        files.sort_by_key(|f| (f.is_dir, std::cmp::Reverse(f.modified)));
        files.truncate(LIST_DIRECTORY_MAX_ENTRIES);
        Ok(files)
    })
    .await
    .map_err(|e| format!("directory list task panicked: {e}"))?
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

        let name = copy_file_into_chat_folder_blocking(
            &src_file.to_string_lossy(),
            &cwd_dir.to_string_lossy(),
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

        let name = copy_file_into_chat_folder_blocking(
            &src_file.to_string_lossy(),
            &cwd_dir.to_string_lossy(),
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

        let name = copy_file_into_chat_folder_blocking(
            &src_file.to_string_lossy(),
            &cwd_dir.to_string_lossy(),
        )
        .unwrap();

        assert_eq!(name, "a.txt");
        assert!(cwd_dir.join("a.txt").exists());
    }

    #[test]
    fn errors_when_the_source_file_does_not_exist() {
        let cwd_dir = temp_subdir("missing_src_cwd");
        let result = copy_file_into_chat_folder_blocking(
            "C:/definitely/does/not/exist.docx",
            &cwd_dir.to_string_lossy(),
        );
        assert!(result.is_err());
    }

    /// Regression (815bugs #8): an oversized attachment must be refused with
    /// a user-safe error *before* any copying happens — a sync command used
    /// to run an unbounded copy on the main thread.
    #[test]
    fn rejects_a_source_over_the_size_cap_without_copying() {
        let src_dir = temp_subdir("cap_src");
        let cwd_dir = temp_subdir("cap_cwd");
        let src_file = src_dir.join("huge.bin");
        {
            let f = std::fs::File::create(&src_file).unwrap();
            // Sparse extension: no real bytes are written, but metadata
            // reports the full length — exactly what the cap check reads.
            f.set_len(COPY_INTO_CHAT_FOLDER_MAX_BYTES + 1).unwrap();
        }

        let err = copy_file_into_chat_folder_blocking(
            &src_file.to_string_lossy(),
            &cwd_dir.to_string_lossy(),
        )
        .unwrap_err();

        assert!(err.contains("too large"), "unexpected error: {err}");
        assert!(
            std::fs::read_dir(&cwd_dir).unwrap().next().is_none(),
            "nothing may be copied into the chat folder"
        );
    }
}

/// The single directory tree the bundled MCP tool plugins are able to reach.
///
/// `kitty-tools` enforces its own path boundary
/// (`plugins/kitty-tools/src/paths.rs::path_within_allowed`) independently of
/// BigTiny's per-session sandbox. This mirrors the *baseline* of that crate's
/// resolution order, so a file this function calls reachable really is:
///
/// - **Android** — `KITTY_PLUGIN_HOME`, which `lifecycle::bigtiny_env` sets to
///   the app-private data directory. Nothing outside it is readable by a tool,
///   and that is essentially everything a user can pick: the document picker
///   hands back paths under `/storage/emulated/0/…` or a content-provider
///   cache, none of which live under the app's own data directory.
/// - **Desktop** — the user's home directory (`%USERPROFILE%` / `$HOME`). Most
///   attachments already qualify; a file on another volume does not, and gets
///   staged into the chat folder instead.
///
/// **This is deliberately a lower bound, not the whole grant set.** The tools'
/// real allowed set is home *plus* temp, the daemon's data root and the
/// session's own grants (`mcp::kitty_grants`), which this process cannot see —
/// they are composed daemon-side and some of them are per-session. Erring
/// narrow is the safe direction: the cost of calling a reachable file
/// unreachable is one needless copy into the chat folder, whereas the reverse
/// hands the model a path it cannot open.
///
/// It used to err the *other* way. `kitty-tools` treated `KITTY_PLUGIN_HOME`
/// as its authorization boundary, and the daemon scopes that per app — so the
/// tools' real wall was a narrow `apps/<id>/plugin-home` while this function,
/// reading Kitty's own environment where the variable is unset, answered with
/// the entire user profile. Every attachment under the home directory was
/// therefore passed through unstaged to a tool that would refuse to open it.
fn tools_reachable_root() -> Option<PathBuf> {
    // Kept as a literal rather than importing the plugin crate's own
    // `PLUGIN_HOME_ENV`: `kitty-tools` is a bundled sidecar, not a dependency
    // of this binary. `lifecycle::bigtiny_env` writes the same literal.
    for key in ["KITTY_PLUGIN_HOME", "USERPROFILE", "HOME"] {
        if let Ok(v) = std::env::var(key) {
            if !v.trim().is_empty() {
                return Some(PathBuf::from(v));
            }
        }
    }
    dirs::home_dir()
}

/// Case-insensitively (on Windows) "is `candidate` inside `base`". Deliberately
/// the same shape as `kitty-tools`' own check, including the trailing separator
/// so a sibling directory cannot alias its neighbour.
fn within_root(base: &std::path::Path, candidate: &std::path::Path) -> bool {
    let norm = |p: &std::path::Path| {
        let s = p.to_string_lossy().replace('\\', "/");
        // Strip Windows' verbatim prefix. `canonicalize` emits `//?/C:/…`, and
        // it only succeeds for a path that exists — so comparing a canonical
        // root against a not-yet-canonicalizable candidate compared `//?/c:/…`
        // with `c:/…` and answered "outside" for a path that was plainly
        // inside. Harmless in direction (it stages a needless copy) but wrong,
        // and it made the containment test unassertable.
        let s = s.strip_prefix("//?/").unwrap_or(&s).to_string();
        let s = s.trim_end_matches('/').to_string();
        if cfg!(windows) {
            s.to_lowercase()
        } else {
            s
        }
    };
    // Canonicalize where possible so a symlinked or 8.3-shortened path is
    // judged by where it actually lands, matching the plugin-side check.
    let base_s = norm(&std::fs::canonicalize(base).unwrap_or_else(|_| base.to_path_buf()));
    let cand_s =
        norm(&std::fs::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf()));
    cand_s == base_s || cand_s.starts_with(&format!("{base_s}/"))
}

/// True when a tool could open this path as-is.
fn reachable_by_tools(path: &str) -> bool {
    // A `content://` URI is not a path on any platform, so no containment test
    // applies and no tool can open it. Answering `false` here rather than only
    // inside the Android branch keeps the two in agreement: if the
    // ContentResolver hop ever fails, the fallback is "unreachable", not
    // "inside the root because the string happened not to start with C:\".
    if is_content_uri(path) {
        return false;
    }
    match tools_reachable_root() {
        // No resolvable root means no boundary to be inside of. Fail closed —
        // staging a copy is always safe, whereas handing over an unreachable
        // path is the bug this exists to fix.
        None => false,
        Some(root) => within_root(&root, std::path::Path::new(path)),
    }
}

/// True for a path that is really an Android `content://` URI rather than a
/// filesystem path.
///
/// Lives here rather than beside the Kotlin boundary in `android::attachments`
/// because that whole module is `#[cfg(target_os = "android")]` — the one
/// piece of pure logic in this fix would then be untestable in the desktop
/// build, which is the only build the test suite runs in.
///
/// Compared case-insensitively: `Uri.parse` treats the scheme that way, so a
/// provider handing back `Content://` must not silently fall through to the
/// filesystem branch, where there is no file to find.
fn is_content_uri(path: &str) -> bool {
    path.len() > "content://".len() && path[.."content://".len()].eq_ignore_ascii_case("content://")
}

/// One attached path as the model should be told about it.
#[derive(Debug, Clone, Serialize)]
pub struct StagedAttachment {
    /// Where the file was originally picked from.
    pub original_path: String,
    /// The path to hand the model — the original when a tool can already open
    /// it, otherwise the copy inside the session's working directory.
    pub path: String,
    /// Basename of `path`.
    pub name: String,
    /// True when a copy was made (the UI says so on the chip).
    pub staged: bool,
}

/// Stage one attached path. Split from the command so the per-path decision
/// is a single expression rather than a closure inside a `map` inside a
/// `spawn_blocking`.
fn stage_one(path: String, cwd: &str) -> StagedAttachment {
    let original_path = path.clone();
    let basename = |s: &str| {
        std::path::Path::new(s)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| s.to_string())
    };
    let passthrough = |p: String| StagedAttachment {
        name: basename(&p),
        path: p,
        original_path: original_path.clone(),
        staged: false,
    };

    // Android's document picker hands back a `content://` URI, which is not a
    // path at all — there is no file to stat, copy, or hand to a tool, and its
    // last segment is the provider's internal document id rather than a name.
    // Only the ContentResolver can answer either question, so this goes out to
    // Kotlin. See `android::attachments`.
    #[cfg(target_os = "android")]
    if is_content_uri(&path) {
        return match crate::android::attachments::copy_into(&path, cwd) {
            Ok(copied) => StagedAttachment {
                original_path,
                path: copied.path,
                name: copied.name,
                staged: true,
            },
            Err(e) => {
                tracing::warn!("could not stage content-uri attachment: {e}");
                passthrough(path)
            }
        };
    }

    if reachable_by_tools(&path) {
        return passthrough(path);
    }
    match copy_file_into_chat_folder_blocking(&path, cwd) {
        Ok(name) => {
            let dest = std::path::Path::new(cwd).join(&name);
            StagedAttachment {
                original_path,
                path: dest.to_string_lossy().to_string(),
                name,
                staged: true,
            }
        }
        // Best-effort: a failed copy leaves the original path in place. The
        // model may still fail to open it, but that is no worse than the
        // behaviour this replaces, and it beats dropping the attachment on the
        // floor.
        Err(e) => {
            tracing::warn!("could not stage attachment {path}: {e}");
            passthrough(path)
        }
    }
}

/// Make user-attached files actually openable by the model's file tools.
///
/// Real, observed bug: attaching a document on Android and asking about it got
/// "I can't find that file", after the model tried `lean_analyze_workspace`
/// several times looking for it. There are two separate causes, and fixing
/// either alone leaves the feature broken:
///
/// 1. **The path is not a path.** Android's document picker returns a
///    `content://` URI. Nothing in Rust can open one, and the model has no
///    tool that can either — its own reasoning trace named the problem
///    exactly: "I don't have a tool to resolve content:// URIs". The URI's
///    last segment is the provider's document id, so it is not a usable name
///    either. `android::attachments` resolves both through the
///    ContentResolver.
/// 2. **The reachable root is narrower than the daemon's sandbox.** BigTiny
///    was never the obstacle: `routes::chat` unions `attached_paths` into the
///    session metadata and `sandbox::allowed_dirs_for_session` honours them.
///    `kitty-tools` enforces its *own* home-directory boundary, which on
///    Android is the app-private data directory — so even a real path from
///    outside it stays unreadable however the daemon is configured.
///
/// Rather than widen a security boundary to fix an ergonomics problem, bring
/// the file to the model: anything unreachable is copied into the session's
/// own working directory, which is inside the boundary by construction
/// (`session::chats_base_dir` falls back to the same app data directory on
/// Android). This is the mechanism `inlineFileAsAttachment` already uses for
/// providers with no file tools; this makes it apply to the tool-using path
/// too, which is where the bug was visible.
///
/// A file already inside the root is passed through untouched — no needless
/// copy of a 2 GB video the tools could have opened where it lay.
#[tauri::command]
pub async fn stage_attachments(
    paths: Vec<String>,
    cwd: String,
) -> Result<Vec<StagedAttachment>, String> {
    // `spawn_blocking` is not just for the copy: on Android the
    // ContentResolver call blocks on the main looper, and running it *from*
    // the main thread deadlocks the app with no error (see
    // `android::secrets`' warning).
    tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .map(|p| stage_one(p, &cwd))
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| format!("attachment staging task panicked: {e}"))
}

#[cfg(test)]
mod staging_tests {
    use super::*;

    #[test]
    fn content_uris_are_recognised_and_real_paths_are_not() {
        // The bug this guards: a picked Android attachment arrives as a URI,
        // not a path, so the filesystem branch can never stage it and the
        // model is handed something no tool can open.
        assert!(is_content_uri(
            "content://com.android.providers.downloads.documents/document/msf%3A1000000123"
        ));
        assert!(is_content_uri("CONTENT://x/y"));

        assert!(!is_content_uri("/data/user/0/com.kitty.app/files/a.pdf"));
        assert!(!is_content_uri("/storage/emulated/0/Download/report.docx"));
        assert!(!is_content_uri("file:///storage/emulated/0/a.pdf"));
        // The scheme on its own names no document.
        assert!(!is_content_uri("content://"));
        assert!(!is_content_uri(""));
    }

    #[test]
    fn a_path_inside_the_root_is_recognised() {
        let dir = std::env::temp_dir().join("kitty-stage-test");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(within_root(&dir, &dir.join("a.txt")));
        assert!(within_root(&dir, &dir));
    }

    #[test]
    fn a_sibling_directory_cannot_alias_the_root() {
        // `…/kitty2` must not count as inside `…/kitty` — the trailing
        // separator is what stops the prefix match from being a string bug.
        let base = std::env::temp_dir().join("kitty");
        let sibling = std::env::temp_dir().join("kitty2").join("f.txt");
        assert!(!within_root(&base, &sibling));
    }

    #[test]
    fn an_unrelated_location_is_outside_the_root() {
        let base = std::env::temp_dir().join("kitty-root");
        assert!(!within_root(&base, std::path::Path::new("/etc/passwd")));
    }
}
