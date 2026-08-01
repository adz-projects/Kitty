//! `lean_file_read`/`write`/`append`/`replace_str`/`replace_lines` — Rust
//! port of `lean_mcp.py`'s file tools.

use crate::envelope::{error_response, success_response};
use crate::paths::resolve;
use crate::query_filter::filter_by_query;
use crate::text::py_splitlines;
use serde_json::json;

const FILE_PAGE_SIZE: usize = 200;

pub fn file_read(path: &str, start_line: Option<i64>, end_line: Option<i64>, query: Option<&str>) -> String {
    let resolved = resolve(path);
    if !resolved.exists() {
        return error_response("FILE_NOT_FOUND", "Path does not exist", Some(&resolved.to_string_lossy()), None);
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
    if !resolved.exists() {
        return error_response("FILE_NOT_FOUND", "Path does not exist", Some(&resolved.to_string_lossy()), None);
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
    if !resolved.exists() {
        return error_response("FILE_NOT_FOUND", "Path does not exist", Some(&resolved.to_string_lossy()), None);
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
}
