//! `lean_file_read`/`write`/`append`/`replace_str`/`replace_lines` — Rust
//! port of `lean_mcp.py`'s file tools.

use crate::envelope::{error_response, success_response};
use crate::paths::path_within_home;
use crate::paths::resolve;
use crate::query_filter::filter_by_query;
use crate::text::py_splitlines;
use serde_json::json;

const FILE_PAGE_SIZE: usize = 200;
/// Hard cap on full-file reads (`file_read`/`file_replace_str`/
/// `file_replace_lines` all materialize the whole file). Text files larger
/// than this are rejected up front rather than read into memory — defense
/// against OOM on a multi-gigabyte file, not a size judgment.
const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;

fn outside_home(resolved: &std::path::Path) -> bool {
    !path_within_home(resolved)
}

/// True when the file's metadata size is past `MAX_FILE_BYTES` — the check
/// that keeps giant files from being materialized into memory at all.
fn file_size_exceeds(resolved: &std::path::Path) -> bool {
    std::fs::metadata(resolved).map(|m| m.len() > MAX_FILE_BYTES as u64).unwrap_or(false)
}

pub fn file_read(path: &str, start_line: Option<i64>, end_line: Option<i64>, query: Option<&str>) -> String {
    let resolved = resolve(path);
    if outside_home(&resolved) {
        return error_response(
            "PATH_OUTSIDE_HOME",
            "Path is outside the HOME directory",
            Some(&resolved.to_string_lossy()),
            Some("Only paths inside your home directory can be accessed."),
        );
    }
    if !resolved.exists() {
        return error_response("FILE_NOT_FOUND", "Path does not exist", Some(&resolved.to_string_lossy()), None);
    }
    if file_size_exceeds(&resolved) {
        return error_response(
            "FILE_TOO_LARGE",
            &format!("File is larger than the {} byte read limit", MAX_FILE_BYTES),
            Some(&resolved.to_string_lossy()),
            Some("Use lean_file_read on a smaller file, or search for it with lean_analyze_workspace first."),
        );
    }

    let text = match std::fs::read_to_string(&resolved) {
        Ok(t) => t,
        Err(e) => return error_response("FILE_READ_ERROR", &format!("Cannot read file: {e}"), Some(&resolved.to_string_lossy()), None),
    };
    let lines = py_splitlines(&text);
    let total_lines = lines.len();

    if let Some(q) = query.filter(|q| !q.trim().is_empty()) {
        let numbered: Vec<String> = lines.iter().enumerate().map(|(idx, l)| format!("{}: {}", idx + 1, l)).collect();
        let result = filter_by_query(&numbered, Some(q), 50, 0);
        let message = result.no_match.then(|| format!("No direct matches for query '{q}'. Showing top section."));
        return success_response(
            json!(result.items.join("\n")),
            message.as_deref(),
            result.truncated,
            Some(json!({"total_lines": total_lines, "filtered_by_query": q})),
        );
    }

    let start_line = start_line.unwrap_or(1).max(1) as usize;
    let window_end = end_line.map(|e| e as usize).unwrap_or(start_line + FILE_PAGE_SIZE - 1);
    let actual_end = window_end.min(total_lines);

    let page: Vec<String> = if start_line <= actual_end && start_line <= total_lines {
        lines[start_line - 1..actual_end]
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{}: {}", start_line + i, l))
            .collect()
    } else {
        Vec::new()
    };
    let has_more = actual_end < total_lines;

    success_response(
        json!(page.join("\n")),
        None,
        has_more,
        Some(json!({
            "start_line": start_line,
            "end_line": actual_end,
            "total_lines": total_lines,
            "has_more": has_more,
        })),
    )
}

pub fn file_write(path: &str, content: &str, dry_run: bool) -> String {
    let resolved = resolve(path);
    if outside_home(&resolved) {
        return error_response(
            "PATH_OUTSIDE_HOME",
            "Path is outside the HOME directory",
            Some(&resolved.to_string_lossy()),
            Some("Only paths inside your home directory can be accessed."),
        );
    }
    if dry_run {
        return success_response(json!({"path": resolved.to_string_lossy()}), Some("[DRY RUN] Would write file."), false, None);
    }
    if let Some(parent) = resolved.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return error_response("FILE_WRITE_ERROR", &format!("Cannot create parent directory: {e}"), None, None);
        }
    }
    if let Err(e) = std::fs::write(&resolved, content) {
        return error_response("FILE_WRITE_ERROR", &format!("Cannot write file: {e}"), None, None);
    }
    success_response(
        json!({"path": resolved.to_string_lossy(), "words": content.split_whitespace().count()}),
        Some("File written successfully."),
        false,
        None,
    )
}

pub fn file_append(path: &str, content: &str, dry_run: bool) -> String {
    let resolved = resolve(path);
    if outside_home(&resolved) {
        return error_response(
            "PATH_OUTSIDE_HOME",
            "Path is outside the HOME directory",
            Some(&resolved.to_string_lossy()),
            Some("Only paths inside your home directory can be accessed."),
        );
    }
    if !resolved.exists() {
        return error_response("FILE_NOT_FOUND", "Path does not exist", Some(&resolved.to_string_lossy()), None);
    }
    if dry_run {
        return success_response(json!({"path": resolved.to_string_lossy()}), Some("[DRY RUN] Would append to file."), false, None);
    }
    use std::io::Write;
    let file = std::fs::OpenOptions::new().append(true).open(&resolved);
    match file {
        Ok(mut f) => {
            if let Err(e) = f.write_all(content.as_bytes()) {
                return error_response("FILE_WRITE_ERROR", &format!("Cannot append to file: {e}"), None, None);
            }
        }
        Err(e) => return error_response("FILE_WRITE_ERROR", &format!("Cannot open file: {e}"), None, None),
    }
    success_response(
        json!({"path": resolved.to_string_lossy(), "appended_words": content.split_whitespace().count()}),
        Some("Content appended successfully."),
        false,
        None,
    )
}

pub fn file_replace_str(path: &str, old_str: &str, new_str: &str, dry_run: bool) -> String {
    let resolved = resolve(path);
    if outside_home(&resolved) {
        return error_response(
            "PATH_OUTSIDE_HOME",
            "Path is outside the HOME directory",
            Some(&resolved.to_string_lossy()),
            Some("Only paths inside your home directory can be accessed."),
        );
    }
    if !resolved.exists() {
        return error_response("FILE_NOT_FOUND", "Path does not exist", Some(&resolved.to_string_lossy()), None);
    }
    // An empty `old_str` is not "zero occurrences" — `str::matches("")` counts
    // every character boundary and `str::replace("", x)` inserts `x` between
    // every character, which would corrupt the file. Reject before any read.
    if old_str.is_empty() {
        return error_response("INVALID_ARGUMENT", "old_str must not be empty.", None, Some("Provide a non-empty string to search for."));
    }
    if file_size_exceeds(&resolved) {
        return error_response(
            "FILE_TOO_LARGE",
            &format!("File is larger than the {} byte read limit", MAX_FILE_BYTES),
            Some(&resolved.to_string_lossy()),
            Some("Use lean_file_replace_str on a smaller file instead."),
        );
    }
    let file_text = match std::fs::read_to_string(&resolved) {
        Ok(t) => t,
        Err(e) => return error_response("FILE_READ_ERROR", &format!("Cannot read file: {e}"), None, None),
    };
    let occurrences = file_text.matches(old_str).count();
    if occurrences == 0 {
        return error_response("TARGET_NOT_FOUND", "Target string 'old_str' was not found in the file.", None, None);
    }
    if dry_run {
        return success_response(
            json!({"occurrences": occurrences, "path": resolved.to_string_lossy()}),
            Some(&format!("[DRY RUN] Would replace {occurrences} occurrence(s).")),
            false,
            None,
        );
    }
    let updated = file_text.replace(old_str, new_str);
    if let Err(e) = std::fs::write(&resolved, updated) {
        return error_response("FILE_WRITE_ERROR", &format!("Cannot write file: {e}"), None, None);
    }
    success_response(
        json!({"path": resolved.to_string_lossy(), "replacements_made": occurrences}),
        Some(&format!("Successfully replaced {occurrences} occurrence(s).")),
        false,
        None,
    )
}

pub fn file_replace_lines(path: &str, start_line: i64, end_line: i64, new_content: &str, dry_run: bool) -> String {
    let resolved = resolve(path);
    if outside_home(&resolved) {
        return error_response(
            "PATH_OUTSIDE_HOME",
            "Path is outside the HOME directory",
            Some(&resolved.to_string_lossy()),
            Some("Only paths inside your home directory can be accessed."),
        );
    }
    if !resolved.exists() {
        return error_response("FILE_NOT_FOUND", "Path does not exist", Some(&resolved.to_string_lossy()), None);
    }
    if file_size_exceeds(&resolved) {
        return error_response(
            "FILE_TOO_LARGE",
            &format!("File is larger than the {} byte read limit", MAX_FILE_BYTES),
            Some(&resolved.to_string_lossy()),
            Some("Use lean_file_replace_lines on a smaller file instead."),
        );
    }
    let text = match std::fs::read_to_string(&resolved) {
        Ok(t) => t,
        Err(e) => return error_response("FILE_READ_ERROR", &format!("Cannot read file: {e}"), None, None),
    };
    let mut lines = py_splitlines(&text);
    let total_lines = lines.len() as i64;

    if start_line < 1 || start_line > total_lines || end_line < start_line {
        return error_response(
            "OUT_OF_BOUNDS",
            &format!("Invalid line range {start_line}-{end_line} for file with {total_lines} lines."),
            None,
            None,
        );
    }

    let actual_end = end_line.min(total_lines) as usize;
    let start_idx = start_line as usize;

    if dry_run {
        return success_response(
            json!({"start_line": start_line, "end_line": actual_end, "total_lines": total_lines}),
            Some("[DRY RUN] Would replace specified line range."),
            false,
            None,
        );
    }

    let new_lines: Vec<String> = if new_content.is_empty() { Vec::new() } else { py_splitlines(new_content) };
    let removed = actual_end - start_idx + 1;
    lines.splice(start_idx - 1..actual_end, new_lines.iter().cloned());

    let mut out = lines.join("\n");
    if !lines.is_empty() {
        out.push('\n');
    }
    if let Err(e) = std::fs::write(&resolved, out) {
        return error_response("FILE_WRITE_ERROR", &format!("Cannot write file: {e}"), None, None);
    }

    success_response(
        json!({
            "path": resolved.to_string_lossy(),
            "replaced_range": format!("{start_line}-{actual_end}"),
            "lines_removed": removed,
            "lines_added": new_lines.len(),
            "new_total_lines": lines.len(),
        }),
        Some("Line range replaced successfully."),
        false,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kt-fs-{}-{}", std::process::id(), name));
        fs::create_dir_all(&dir).unwrap();
        dir.join("f.txt")
    }

    #[test]
    fn write_then_read_round_trips() {
        let f = tmp("rw");
        file_write(f.to_str().unwrap(), "line1\nline2\nline3", false);
        let s = file_read(f.to_str().unwrap(), None, None, None);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(v["data"].as_str().unwrap().contains("1: line1"));
        assert_eq!(v["metadata"]["total_lines"], 3);
        fs::remove_dir_all(f.parent().unwrap()).ok();
    }

    #[test]
    fn replace_str_reports_target_not_found() {
        let f = tmp("rs");
        file_write(f.to_str().unwrap(), "hello world", false);
        let s = file_replace_str(f.to_str().unwrap(), "nonexistent", "x", false);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error_code"], "TARGET_NOT_FOUND");
        fs::remove_dir_all(f.parent().unwrap()).ok();
    }

    #[test]
    fn replace_lines_out_of_bounds() {
        let f = tmp("rl");
        file_write(f.to_str().unwrap(), "a\nb\nc", false);
        let s = file_replace_lines(f.to_str().unwrap(), 10, 12, "x", false);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error_code"], "OUT_OF_BOUNDS");
        fs::remove_dir_all(f.parent().unwrap()).ok();
    }

    #[test]
    fn replace_lines_replaces_range() {
        let f = tmp("rl2");
        file_write(f.to_str().unwrap(), "a\nb\nc\nd", false);
        file_replace_lines(f.to_str().unwrap(), 2, 3, "X\nY\nZ", false);
        let text = fs::read_to_string(&f).unwrap();
        assert_eq!(text, "a\nX\nY\nZ\nd\n");
        fs::remove_dir_all(f.parent().unwrap()).ok();
    }

    #[test]
    fn append_requires_existing_file() {
        let f = tmp("ap");
        let s = file_append(f.to_str().unwrap(), "x", false);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error_code"], "FILE_NOT_FOUND");
        fs::remove_dir_all(f.parent().unwrap()).ok();
    }

    #[test]
    fn absolute_paths_outside_home_are_rejected() {
        #[cfg(windows)]
        let p = "C:\\Windows\\System32\\drivers\\etc\\hosts";
        #[cfg(not(windows))]
        let p = "/etc/passwd";

        let s = file_read(p, None, None, None);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error_code"], "PATH_OUTSIDE_HOME");
        assert_eq!(v["status"], "error");

        let s = file_write(p, "x", false);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error_code"], "PATH_OUTSIDE_HOME");

        let s = file_append(p, "x", false);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error_code"], "PATH_OUTSIDE_HOME");

        let s = file_replace_str(p, "a", "b", false);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error_code"], "PATH_OUTSIDE_HOME");

        let s = file_replace_lines(p, 1, 2, "x", false);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error_code"], "PATH_OUTSIDE_HOME");

        // Dry-run must reject too — authorization happens before any write.
        let s = file_write(p, "x", true);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error_code"], "PATH_OUTSIDE_HOME");
    }

    #[test]
    fn empty_old_str_is_rejected_before_any_write() {
        let f = tmp("empty");
        file_write(f.to_str().unwrap(), "hello world", false);
        let s = file_replace_str(f.to_str().unwrap(), "", "X", false);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error_code"], "INVALID_ARGUMENT");
        // File must be untouched.
        assert_eq!(fs::read_to_string(&f).unwrap(), "hello world");
        fs::remove_dir_all(f.parent().unwrap()).ok();
    }

    #[test]
    fn oversized_file_is_rejected_without_reading() {
        let f = tmp("big");
        let big = "x".repeat(MAX_FILE_BYTES + 1);
        fs::write(&f, big).unwrap();

        let s = file_read(f.to_str().unwrap(), None, None, None);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error_code"], "FILE_TOO_LARGE");

        let s = file_replace_str(f.to_str().unwrap(), "x", "y", false);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error_code"], "FILE_TOO_LARGE");

        let s = file_replace_lines(f.to_str().unwrap(), 1, 2, "z", false);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error_code"], "FILE_TOO_LARGE");

        fs::remove_dir_all(f.parent().unwrap()).ok();
    }
}
