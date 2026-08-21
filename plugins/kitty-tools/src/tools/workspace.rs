//! `lean_analyze_workspace` — Rust port of `lean_mcp.py`'s `analyze_workspace`.

use std::path::Path;

use crate::envelope::{error_response, success_response};
use crate::paths::path_within_home;
use crate::paths::resolve;
use serde_json::json;

const WORKSPACE_MAX_DEPTH: u32 = 10;
const WORKSPACE_MAX_FILES: usize = 150;
const WORKSPACE_MAX_DIRS: usize = 500;

fn blacklisted(name: &str) -> bool {
    matches!(
        name,
        ".git" | "node_modules" | "__pycache__" | "venv" | ".venv" | "dist" | "build" | ".tox"
    )
}

/// The directory this walk actually starts from.
///
/// On desktop, whatever the caller asked for. **On Android, anything outside
/// the app's own storage is redirected to it**, because scoped storage means
/// a path the model picked — `/sdcard/Documents`, `/storage/emulated/0`, some
/// remembered Windows path — is either unreadable or returns almost nothing,
/// and a near-empty listing reads to the model as "this directory is empty"
/// rather than "you cannot see this". Walking the one tree the app can
/// actually read is the honest answer, and the response's `root` says which
/// directory that was (docs/ANDROID.md §2.4).
fn scoped_root(path: &str) -> std::path::PathBuf {
    let resolved = resolve(path);
    #[cfg(target_os = "android")]
    {
        if !path_within_home(&resolved) {
            if let Some(home) = crate::paths::home_dir() {
                return home;
            }
        }
    }
    resolved
}

pub fn analyze_workspace(path: &str, max_depth: Option<u32>) -> String {
    let resolved = scoped_root(path);
    if !path_within_home(&resolved) {
        return error_response(
            "PATH_OUTSIDE_HOME",
            "Path is outside the HOME directory",
            Some(&resolved.to_string_lossy()),
            Some("Only paths inside your home directory can be accessed."),
        );
    }
    if !resolved.exists() {
        return error_response(
            "PATH_NOT_FOUND",
            "Directory does not exist",
            Some(&resolved.to_string_lossy()),
            None,
        );
    }

    if resolved.is_file() {
        let size = std::fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0);
        let name = resolved
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        return success_response(
            json!({
                "type": "file",
                "name": name,
                "size_bytes": size,
                "path": resolved.to_string_lossy(),
            }),
            None,
            false,
            None,
        );
    }

    // Clamp a caller-supplied `max_depth` so a huge value can't walk
    // arbitrarily deep — the default is already the ceiling.
    let depth = max_depth
        .unwrap_or(WORKSPACE_MAX_DEPTH)
        .min(WORKSPACE_MAX_DEPTH);
    let mut found = Found::default();
    walk(&resolved, &resolved, 0, depth, &mut found);
    let Found {
        files,
        dirs,
        symlinks,
        abort,
    } = found;

    // `symlinks` is reported separately rather than folded into `files`: they
    // are not descended into (see `walk`), so calling a symlinked directory a
    // file would be a plain lie to the model about what it is looking at.
    let mut data = serde_json::Map::new();
    data.insert("files".into(), json!(files));
    data.insert("directories".into(), json!(dirs));
    if !symlinks.is_empty() {
        data.insert("symlinks".into(), json!(symlinks));
    }
    let mut meta = serde_json::Map::new();
    meta.insert("total_files".into(), json!(files.len()));
    meta.insert("total_directories".into(), json!(dirs.len()));
    if !symlinks.is_empty() {
        meta.insert("total_symlinks".into(), json!(symlinks.len()));
        meta.insert(
            "symlinks_note".into(),
            json!("Symlinks are listed but not followed, so nothing outside this tree is walked."),
        );
    }
    meta.insert("root".into(), json!(resolved.to_string_lossy()));

    success_response(
        serde_json::Value::Object(data),
        None,
        abort,
        Some(serde_json::Value::Object(meta)),
    )
}

/// What the walk has collected so far, plus whether it stopped early.
/// Grouped rather than passed as four `&mut` parameters threaded through the
/// recursion.
#[derive(Default)]
struct Found {
    files: Vec<String>,
    dirs: Vec<String>,
    /// Listed but never descended into — see the note in `walk`.
    symlinks: Vec<String>,
    /// Set when a budget was hit, so the response can flag itself truncated.
    abort: bool,
}

fn walk(root: &Path, current: &Path, current_depth: u32, max_depth: u32, found: &mut Found) {
    if found.abort || current_depth > max_depth {
        return;
    }

    let mut entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(current) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return,
    };

    // Python: `sorted(current.iterdir(), key=lambda e: (e.is_file(), e.name.lower()))`
    // — directories first (False < True), then case-insensitive name.
    entries.sort_by_key(|e| {
        let is_file = e.file_type().map(|t| t.is_file()).unwrap_or(false);
        let name = e.file_name().to_string_lossy().to_lowercase();
        (is_file, name)
    });

    for entry in entries {
        if found.abort {
            return;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if blacklisted(&name) {
            continue;
        }
        let entry_path = entry.path();
        let rel = entry_path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or(name);

        // `file_type()` from `read_dir` does **not** follow symlinks, and that
        // is deliberate: following one would let a link inside home produce a
        // listing of anything outside it, since the home boundary is only
        // checked on the root. But the un-followed type also made a symlinked
        // *directory* come back as `is_dir() == false`, so it was silently
        // filed under `files` and counted against that budget. Report it as
        // what it is instead of mislabelling it.
        let file_type = entry.file_type();
        let is_symlink = file_type.as_ref().map(|t| t.is_symlink()).unwrap_or(false);
        let is_dir = file_type.map(|t| t.is_dir()).unwrap_or(false);

        if is_symlink {
            found.symlinks.push(rel);
            if found.symlinks.len() >= WORKSPACE_MAX_FILES {
                found.abort = true;
                return;
            }
            continue;
        }
        if is_dir {
            found.dirs.push(rel);
            // Cap collected dirs too — a huge empty tree (all dirs, no files)
            // used to be able to run forever because the abort only fired on
            // the file count.
            if found.dirs.len() >= WORKSPACE_MAX_DIRS {
                found.abort = true;
                return;
            }
            walk(root, &entry_path, current_depth + 1, max_depth, found);
        } else {
            found.files.push(rel);
            if found.files.len() >= WORKSPACE_MAX_FILES {
                found.abort = true;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn reports_file_metadata_for_a_single_file() {
        let dir = std::env::temp_dir().join(format!("kt-ws-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        fs::write(&file, "hello").unwrap();

        let s = analyze_workspace(file.to_str().unwrap(), None);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["data"]["type"], "file");
        assert_eq!(v["data"]["size_bytes"], 5);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_path_reports_not_found() {
        // Must be inside home for the boundary check to pass through to the
        // not-found path.
        let dir = std::env::temp_dir().join(format!("kt-ws-missing-{}", std::process::id()));
        let missing = dir.join("never-created");
        let s = analyze_workspace(missing.to_str().unwrap(), None);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["status"], "error");
        assert_eq!(v["error_code"], "PATH_NOT_FOUND");
    }

    #[test]
    fn outside_home_path_is_rejected() {
        #[cfg(windows)]
        let p = "C:\\Windows";
        #[cfg(not(windows))]
        let p = "/etc";
        let s = analyze_workspace(p, None);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error_code"], "PATH_OUTSIDE_HOME");
    }

    #[test]
    fn explicit_max_depth_is_clamped() {
        let root = std::env::temp_dir().join(format!("kt-ws-depth-{}", std::process::id()));
        let mut cur = root.clone();
        fs::create_dir_all(&cur).unwrap();
        for i in 0..12 {
            cur = cur.join(format!("l{i}"));
            fs::create_dir_all(&cur).unwrap();
        }
        let s = analyze_workspace(root.to_str().unwrap(), Some(1000));
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["status"], "success");
        let total = v["metadata"]["total_directories"].as_u64().unwrap();
        // 12 levels exist; the clamp to WORKSPACE_MAX_DEPTH (10) must stop
        // short of walking them all.
        assert!(
            total < 12,
            "depth not clamped: walked {total} levels with clamp {WORKSPACE_MAX_DEPTH}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn huge_empty_tree_aborts_on_dirs_cap() {
        let root = std::env::temp_dir().join(format!("kt-ws-dirs-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        for i in 0..(WORKSPACE_MAX_DIRS + 100) {
            fs::create_dir_all(root.join(format!("d{i}"))).unwrap();
        }
        let s = analyze_workspace(root.to_str().unwrap(), None);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["truncated"], true);
        assert!(
            v["metadata"]["total_directories"].as_u64().unwrap() <= WORKSPACE_MAX_DIRS as u64,
            "dirs not capped"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn blacklisted_dirs_are_skipped() {
        let dir = std::env::temp_dir().join(format!("kt-ws2-{}", std::process::id()));
        let git = dir.join(".git");
        fs::create_dir_all(&git).unwrap();
        fs::write(git.join("config"), "x").unwrap();
        fs::write(dir.join("keep.txt"), "x").unwrap();

        let s = analyze_workspace(dir.to_str().unwrap(), None);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let files: Vec<String> = v["data"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap().to_string())
            .collect();
        assert!(files.iter().any(|f| f.contains("keep.txt")));
        assert!(!files.iter().any(|f| f.contains(".git")));

        fs::remove_dir_all(&dir).ok();
    }

    /// A symlinked *directory* used to be reported as a file: `read_dir`'s
    /// `file_type()` does not follow links, so `is_dir()` was false and it
    /// fell into the `files` bucket. Not following remains correct — a link
    /// inside home pointing outside it would otherwise leak a listing of
    /// wherever it points — but it must be *labelled* for what it is.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_is_listed_as_a_symlink_not_a_file() {
        let root = std::env::temp_dir().join(format!("kt-ws-link-{}", std::process::id()));
        let real = root.join("real");
        std::fs::create_dir_all(real.join("inner")).unwrap();
        std::fs::write(real.join("inner").join("secret.txt"), "x").unwrap();
        let link = root.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let out = analyze_workspace(root.to_str().unwrap(), Some(3));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let symlinks = v["data"]["symlinks"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let files = v["data"]["files"].as_array().cloned().unwrap_or_default();

        assert!(
            symlinks.iter().any(|s| s.as_str() == Some("link")),
            "the symlink must be listed as one: {v}"
        );
        assert!(
            !files.iter().any(|f| f.as_str() == Some("link")),
            "and must not be mislabelled as a file: {v}"
        );
        // Not followed: the linked tree's contents must not appear twice.
        assert!(
            !files
                .iter()
                .any(|f| f.as_str().unwrap_or("").starts_with("link")),
            "the symlink must not be descended into: {v}"
        );

        std::fs::remove_dir_all(&root).ok();
    }
}
