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

pub fn analyze_workspace(path: &str, max_depth: Option<u32>) -> String {
    let resolved = resolve(path);
    if !path_within_home(&resolved) {
        return error_response(
            "PATH_OUTSIDE_HOME",
            "Path is outside the HOME directory",
            Some(&resolved.to_string_lossy()),
            Some("Only paths inside your home directory can be accessed."),
        );
    }
    if !resolved.exists() {
        return error_response("PATH_NOT_FOUND", "Directory does not exist", Some(&resolved.to_string_lossy()), None);
    }

    if resolved.is_file() {
        let size = std::fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0);
        let name = resolved.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
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
    let depth = max_depth.unwrap_or(WORKSPACE_MAX_DEPTH).min(WORKSPACE_MAX_DEPTH);
    let mut files: Vec<String> = Vec::new();
    let mut dirs: Vec<String> = Vec::new();
    let mut abort = false;

    walk(&resolved, &resolved, 0, depth, &mut files, &mut dirs, &mut abort);

    success_response(
        json!({"files": files, "directories": dirs}),
        None,
        abort,
        Some(json!({
            "total_files": files.len(),
            "total_directories": dirs.len(),
            "root": resolved.to_string_lossy(),
        })),
    )
}

fn walk(
    root: &Path,
    current: &Path,
    current_depth: u32,
    max_depth: u32,
    files: &mut Vec<String>,
    dirs: &mut Vec<String>,
    abort: &mut bool,
) {
    if *abort || current_depth > max_depth {
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
        if *abort {
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

        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            dirs.push(rel);
            // Cap collected dirs too — a huge empty tree (all dirs, no files)
            // used to be able to run forever because the abort only fired on
            // the file count.
            if dirs.len() >= WORKSPACE_MAX_DIRS {
                *abort = true;
                return;
            }
            walk(root, &entry_path, current_depth + 1, max_depth, files, dirs, abort);
        } else {
            files.push(rel);
            if files.len() >= WORKSPACE_MAX_FILES {
                *abort = true;
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
        assert!(total < 12, "depth not clamped: walked {total} levels with clamp {WORKSPACE_MAX_DEPTH}");
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
        let files: Vec<String> = v["data"]["files"].as_array().unwrap().iter().map(|f| f.as_str().unwrap().to_string()).collect();
        assert!(files.iter().any(|f| f.contains("keep.txt")));
        assert!(!files.iter().any(|f| f.contains(".git")));

        fs::remove_dir_all(&dir).ok();
    }
}
