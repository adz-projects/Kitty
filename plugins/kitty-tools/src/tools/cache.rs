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
use crate::paths::path_within_home;
use crate::tools::cache_dir;
use serde_json::json;

/// Cap on `cache_view`'s `read_to_string` — a cached file past this is read
/// truncated (with the `truncated` flag set) instead of materialized whole.
const CACHE_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Windows device basenames (compared case-insensitively, up to the first
/// dot): joining `NUL` or `CON.txt` under the cache dir opens the *device*,
/// not a file inside it (audit #126).
const WINDOWS_RESERVED_STEMS: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

fn rejects_traversal(filename: &str) -> bool {
    // `:` is rejected too: on NTFS `file.txt:stream` names an alternate data
    // stream, which would read/write outside the plain cache file (audit
    // #126).
    filename.contains('/')
        || filename.contains('\\')
        || filename.contains("..")
        || filename.contains(':')
        || WINDOWS_RESERVED_STEMS.contains(
            &filename
                .split('.')
                .next()
                .unwrap_or("")
                .to_uppercase()
                .as_str(),
        )
}

/// Defense-in-depth home boundary on a filename already joined under the
/// cache dir — the join is inside home by construction, but a hostile home
/// override should not redirect reads/writes out of it.
fn ensure_within_home(file_path: &std::path::Path) -> Option<String> {
    if !path_within_home(file_path) {
        return Some(error_response(
            "PATH_OUTSIDE_HOME",
            "Path is outside the HOME directory",
            Some(&file_path.to_string_lossy()),
            Some("Only paths inside your home directory can be accessed."),
        ));
    }
    None
}

pub fn cache_list() -> String {
    let dir = cache_dir();
    if !dir.exists() {
        return success_response(json!([]), None, false, None);
    }
    let mut entries: Vec<std::path::PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect(),
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
    cache_view_in(cache_dir(), filename)
}

/// Split for testability — `cache_view` uses the process-global cache dir;
/// the truncation test needs to plant an oversized file in a scratch dir.
fn cache_view_in(dir: std::path::PathBuf, filename: &str) -> String {
    if rejects_traversal(filename) {
        return error_response(
            "CACHE_INVALID_FILENAME",
            "Filename must not contain path separators or '..'.",
            None,
            None,
        );
    }
    let file_path = dir.join(filename);
    if let Some(err) = ensure_within_home(&file_path) {
        return err;
    }
    if !file_path.exists() {
        return error_response(
            "CACHE_MISS",
            &format!("File '{filename}' not found."),
            None,
            None,
        );
    }
    if std::fs::metadata(&file_path)
        .map(|m| m.len() > CACHE_MAX_BYTES)
        .unwrap_or(false)
    {
        use std::io::Read;
        let f = match std::fs::File::open(&file_path) {
            Ok(f) => f,
            Err(e) => {
                return error_response(
                    "CACHE_READ_ERROR",
                    &format!("Cannot read cached file: {e}"),
                    None,
                    None,
                )
            }
        };
        let mut buf = Vec::with_capacity(CACHE_MAX_BYTES as usize / 2);
        if f.take(CACHE_MAX_BYTES).read_to_end(&mut buf).is_err() {
            return error_response("CACHE_READ_ERROR", "Cannot read cached file", None, None);
        }
        let text = String::from_utf8_lossy(&buf);
        return success_response(
            json!(text),
            Some("File was larger than the read limit and was truncated."),
            true,
            Some(json!({"truncated_at_bytes": CACHE_MAX_BYTES})),
        );
    }
    match std::fs::read_to_string(&file_path) {
        Ok(text) => success_response(json!(text), None, false, None),
        Err(e) => error_response(
            "CACHE_READ_ERROR",
            &format!("Cannot read cached file: {e}"),
            None,
            None,
        ),
    }
}

pub fn cache_delete(filename: &str) -> String {
    if rejects_traversal(filename) {
        return error_response(
            "CACHE_INVALID_FILENAME",
            "Filename must not contain path separators or '..'.",
            None,
            None,
        );
    }
    let file_path = cache_dir().join(filename);
    if let Some(err) = ensure_within_home(&file_path) {
        return err;
    }
    if file_path.exists() {
        if let Err(e) = std::fs::remove_file(&file_path) {
            return error_response(
                "CACHE_DELETE_ERROR",
                &format!("Cannot delete cached file: {e}"),
                None,
                None,
            );
        }
        return success_response(json!({"deleted": filename}), None, false, None);
    }
    error_response(
        "CACHE_MISS",
        &format!("File '{filename}' not found."),
        None,
        None,
    )
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
    fn ads_and_device_names_are_rejected() {
        // Audit #126: `file.txt:stream` is an NTFS alternate data stream and
        // `NUL`/`CON.txt` are devices, not files inside the cache dir.
        for bad in [
            "notes.txt:secret",
            "C:",
            "NUL",
            "nul.txt",
            "CON",
            "COM1",
            "lpt9.log",
        ] {
            let s = cache_view(bad);
            let v: serde_json::Value = serde_json::from_str(&s).unwrap();
            assert_eq!(
                v["error_code"], "CACHE_INVALID_FILENAME",
                "{bad} must be rejected"
            );
        }
        // Lookalikes are still fine.
        assert!(!rejects_traversal("console.log"));
        assert!(!rejects_traversal("null-values.txt"));
        assert!(!rejects_traversal("report-final.txt"));
    }

    #[test]
    fn miss_reports_cache_miss() {
        let s = cache_view("definitely-not-a-real-cached-file.txt");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error_code"], "CACHE_MISS");
    }

    #[test]
    fn oversized_cached_file_is_read_truncated_with_flag() {
        let dir = std::env::temp_dir().join(format!("kt-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("big.txt"),
            "x".repeat(CACHE_MAX_BYTES as usize + 1),
        )
        .unwrap();

        let s = cache_view_in(dir.clone(), "big.txt");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["status"], "success", "{s}");
        assert_eq!(v["truncated"], true);
        let text = v["data"].as_str().unwrap();
        assert!(
            text.len() <= CACHE_MAX_BYTES as usize,
            "read {} bytes past the {} cap",
            text.len(),
            CACHE_MAX_BYTES
        );
        assert!(v["message"].as_str().unwrap().contains("truncate"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
