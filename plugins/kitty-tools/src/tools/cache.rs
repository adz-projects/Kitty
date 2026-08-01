//! `lean_cache_list`/`view`/`delete`/`clear` — Rust port of `lean_mcp.py`'s
//! cache manager tools.
//!
//! Deliberate deviation (base plan's "Deliberate behavioral deviations"
//! table): `cache_view`/`cache_delete` reject any filename containing a path
//! separator or `..` before joining it under `CACHE_DIR`. The Python
//! original does `CACHE_DIR / filename` with no such check, so
//! `../../../../some/file` escapes the cache directory entirely for
//! arbitrary read/delete — a real path-traversal hole, not ported forward.

use crate::envelope::{error_response, success_response};
use crate::tools::cache_dir;
use serde_json::json;

fn rejects_traversal(filename: &str) -> bool {
    filename.contains('/') || filename.contains('\\') || filename.contains("..")
}

pub fn cache_list() -> String {
    let dir = cache_dir();
    if !dir.exists() {
        return success_response(json!([]), None, false, None);
    }
    let mut entries: Vec<std::path::PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_file()).collect(),
        Err(_) => return success_response(json!([]), None, false, None),
    };
    // Python: `sorted(CACHE_DIR.iterdir())` sorts by full path, not filename.
    entries.sort();
    let out: Vec<_> = entries
        .iter()
        .map(|p| {
            let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            json!({"filename": p.file_name().unwrap_or_default().to_string_lossy(), "size_bytes": size})
        })
        .collect();
    success_response(json!(out), None, false, None)
}

pub fn cache_view(filename: &str) -> String {
    if rejects_traversal(filename) {
        return error_response("CACHE_INVALID_FILENAME", "Filename must not contain path separators or '..'.", None, None);
    }
    let file_path = cache_dir().join(filename);
    if !file_path.exists() {
        return error_response("CACHE_MISS", &format!("File '{filename}' not found."), None, None);
    }
    match std::fs::read_to_string(&file_path) {
        Ok(text) => success_response(json!(text), None, false, None),
        Err(e) => error_response("CACHE_READ_ERROR", &format!("Cannot read cached file: {e}"), None, None),
    }
}

pub fn cache_delete(filename: &str) -> String {
    if rejects_traversal(filename) {
        return error_response("CACHE_INVALID_FILENAME", "Filename must not contain path separators or '..'.", None, None);
    }
    let file_path = cache_dir().join(filename);
    if file_path.exists() {
        if let Err(e) = std::fs::remove_file(&file_path) {
            return error_response("CACHE_DELETE_ERROR", &format!("Cannot delete cached file: {e}"), None, None);
        }
        return success_response(json!({"deleted": filename}), None, false, None);
    }
    error_response("CACHE_MISS", &format!("File '{filename}' not found."), None, None)
}

pub fn cache_clear() -> String {
    let dir = cache_dir();
    let mut count = 0u32;
    if dir.exists() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.filter_map(|e| e.ok()) {
                let p = entry.path();
                if p.is_file() && std::fs::remove_file(&p).is_ok() {
                    count += 1;
                }
            }
        }
    }
    success_response(json!({"files_removed": count}), None, false, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_attempt_is_rejected() {
        let s = cache_view("../../../../etc/passwd");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error_code"], "CACHE_INVALID_FILENAME");

        let s = cache_delete("..\\..\\windows\\system32\\drivers\\etc\\hosts");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error_code"], "CACHE_INVALID_FILENAME");
    }

    #[test]
    fn miss_reports_cache_miss() {
        let s = cache_view("definitely-not-a-real-cached-file.txt");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error_code"], "CACHE_MISS");
    }
}
