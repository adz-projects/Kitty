//! `lean_file_read`/`write`/`append`/`replace_str`/`replace_lines` — Rust
//! port of `lean_mcp.py`'s file tools.

use crate::doc_store::{self, Extraction};
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
    std::fs::metadata(resolved)
        .map(|m| m.len() > MAX_FILE_BYTES as u64)
        .unwrap_or(false)
}

pub fn file_read(
    path: &str,
    start_line: Option<i64>,
    end_line: Option<i64>,
    query: Option<&str>,
) -> String {
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
        return error_response(
            "FILE_NOT_FOUND",
            "Path does not exist",
            Some(&resolved.to_string_lossy()),
            None,
        );
    }
    if file_size_exceeds(&resolved) {
        return error_response(
            "FILE_TOO_LARGE",
            &format!("File is larger than the {} byte read limit", MAX_FILE_BYTES),
            Some(&resolved.to_string_lossy()),
            Some("Use lean_file_read on a smaller file, or search for it with lean_analyze_workspace first."),
        );
    }

    // Read and split once, cached by (path, len, mtime) — see `doc_store`.
    // A plain text file is cheap to re-read, so the win here is not the I/O:
    // it is that a text file gets a `document_id` on the same terms as a PDF
    // or a .docx, so one read loop (`lean_doc_read_chunk`/`lean_doc_search`)
    // works across every document kind instead of three per-kind paginations.
    let doc = match doc_store::ensure(&resolved, doc_store::UNIT_LINE, || {
        let text = std::fs::read_to_string(&resolved).map_err(|e| e.to_string())?;
        let numbered: Vec<String> = py_splitlines(&text)
            .iter()
            .enumerate()
            .map(|(idx, l)| format!("{}: {}", idx + 1, l))
            .collect();
        Ok::<_, String>(Extraction::new(numbered, Vec::new()))
    }) {
        Ok((doc, _persisted)) => doc,
        Err(e) => {
            return error_response(
                "FILE_READ_ERROR",
                &format!("Cannot read file: {e}"),
                Some(&resolved.to_string_lossy()),
                None,
            )
        }
    };
    let lines = &doc.units;
    let total_lines = doc.total_units;

    if let Some(q) = query.filter(|q| !q.trim().is_empty()) {
        let result = filter_by_query(lines, Some(q), 50, 0);
        let message = result
            .no_match
            .then(|| format!("No direct matches for query '{q}'. Showing top section."));
        // The query searches the *whole* file, so any `start_line`/`end_line`
        // the caller also passed does not apply. That has always been the
        // behaviour; what it never did was say so, leaving a caller who sent
        // both to conclude the range was honoured and the file simply had no
        // matches outside it.
        let mut meta = serde_json::Map::new();
        meta.insert("document_id".into(), json!(doc.document_id));
        meta.insert("total_lines".into(), json!(total_lines));
        meta.insert("filtered_by_query".into(), json!(q));
        if start_line.is_some() || end_line.is_some() {
            meta.insert("line_range_ignored".into(), json!(true));
            meta.insert(
                "line_range_ignored_reason".into(),
                json!("query searches the whole file; start_line/end_line apply only to an unfiltered read"),
            );
        }
        return success_response(
            json!(result.items.join("\n")),
            message.as_deref(),
            result.truncated,
            Some(serde_json::Value::Object(meta)),
        );
    }

    // Line numbers are 1-based; anything below 1 is not a line number.
    // `end_line < 1` used to wrap through `as usize` into ~2^64 (silently
    // reading to EOF), and `start_line = i64::MAX` overflowed the window add
    // (audit #123) — both are clamped/saturated here instead.
    let start_line = start_line.unwrap_or(1).max(1) as usize;
    let window_end = end_line
        .filter(|e| *e >= 1)
        .and_then(|e| usize::try_from(e).ok())
        .unwrap_or_else(|| start_line.saturating_add(FILE_PAGE_SIZE - 1));
    let actual_end = window_end.min(total_lines);

    // Lines are stored already numbered, so the window is a plain slice.
    let page: &[String] = if start_line <= actual_end && start_line <= total_lines {
        &lines[start_line - 1..actual_end]
    } else {
        &[]
    };
    let has_more = actual_end < total_lines;

    let message = has_more.then(|| {
        format!(
            "Showing lines {start_line}-{actual_end} of {total_lines}. The whole file is already \
             read and cached — continue with lean_doc_read_chunk (document_id, offset \
             {actual_end}) or search it with lean_doc_search."
        )
    });
    success_response(
        json!(page.join("\n")),
        message.as_deref(),
        has_more,
        Some(json!({
            "document_id": doc.document_id,
            "unit": doc.unit,
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
        return success_response(
            json!({"path": resolved.to_string_lossy()}),
            Some("[DRY RUN] Would write file."),
            false,
            None,
        );
    }
    if let Some(parent) = resolved.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return error_response(
                "FILE_WRITE_ERROR",
                &format!("Cannot create parent directory: {e}"),
                None,
                None,
            );
        }
    }
    if let Err(e) = std::fs::write(&resolved, content) {
        return error_response(
            "FILE_WRITE_ERROR",
            &format!("Cannot write file: {e}"),
            None,
            None,
        );
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
        return error_response(
            "FILE_NOT_FOUND",
            "Path does not exist",
            Some(&resolved.to_string_lossy()),
            None,
        );
    }
    if dry_run {
        return success_response(
            json!({"path": resolved.to_string_lossy()}),
            Some("[DRY RUN] Would append to file."),
            false,
            None,
        );
    }
    use std::io::Write;
    let file = std::fs::OpenOptions::new().append(true).open(&resolved);
    match file {
        Ok(mut f) => {
            if let Err(e) = f.write_all(content.as_bytes()) {
                return error_response(
                    "FILE_WRITE_ERROR",
                    &format!("Cannot append to file: {e}"),
                    None,
                    None,
                );
            }
        }
        Err(e) => {
            return error_response(
                "FILE_WRITE_ERROR",
                &format!("Cannot open file: {e}"),
                None,
                None,
            )
        }
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
        return error_response(
            "FILE_NOT_FOUND",
            "Path does not exist",
            Some(&resolved.to_string_lossy()),
            None,
        );
    }
    // An empty `old_str` is not "zero occurrences" — `str::matches("")` counts
    // every character boundary and `str::replace("", x)` inserts `x` between
    // every character, which would corrupt the file. Reject before any read.
    if old_str.is_empty() {
        return error_response(
            "INVALID_ARGUMENT",
            "old_str must not be empty.",
            None,
            Some("Provide a non-empty string to search for."),
        );
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
        Err(e) => {
            return error_response(
                "FILE_READ_ERROR",
                &format!("Cannot read file: {e}"),
                None,
                None,
            )
        }
    };
    let occurrences = file_text.matches(old_str).count();
    if occurrences == 0 {
        return error_response(
            "TARGET_NOT_FOUND",
            "Target string 'old_str' was not found in the file.",
            None,
            None,
        );
    }
    if dry_run {
        return success_response(
            json!({"occurrences": occurrences, "path": resolved.to_string_lossy()}),
            Some(&format!(
                "[DRY RUN] Would replace {occurrences} occurrence(s)."
            )),
            false,
            None,
        );
    }
    let updated = file_text.replace(old_str, new_str);
    if let Err(e) = std::fs::write(&resolved, updated) {
        return error_response(
            "FILE_WRITE_ERROR",
            &format!("Cannot write file: {e}"),
            None,
            None,
        );
    }
    success_response(
        json!({"path": resolved.to_string_lossy(), "replacements_made": occurrences}),
        Some(&format!(
            "Successfully replaced {occurrences} occurrence(s)."
        )),
        false,
        None,
    )
}

pub fn file_replace_lines(
    path: &str,
    start_line: i64,
    end_line: i64,
    new_content: &str,
    dry_run: bool,
) -> String {
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
        return error_response(
            "FILE_NOT_FOUND",
            "Path does not exist",
            Some(&resolved.to_string_lossy()),
            None,
        );
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
        Err(e) => {
            return error_response(
                "FILE_READ_ERROR",
                &format!("Cannot read file: {e}"),
                None,
                None,
            )
        }
    };
    let mut lines = py_splitlines(&text);
    let total_lines = lines.len() as i64;

    if start_line < 1 || start_line > total_lines || end_line < start_line {
        return error_response(
            "OUT_OF_BOUNDS",
            &format!(
                "Invalid line range {start_line}-{end_line} for file with {total_lines} lines."
            ),
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

    let new_lines: Vec<String> = if new_content.is_empty() {
        Vec::new()
    } else {
        py_splitlines(new_content)
    };
    let removed = actual_end - start_idx + 1;
    lines.splice(start_idx - 1..actual_end, new_lines.iter().cloned());

    // Preserve the file's own EOL style and trailing-newline state (audit
    // #128): joining with "\n" used to convert CRLF files to LF and force a
    // trailing newline onto files that didn't have one — a whole-file
    // mangling for a two-line edit.
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let had_trailing_newline = text.ends_with('\n') || text.ends_with('\r');
    let mut out = lines.join(eol);
    if had_trailing_newline && !lines.is_empty() {
        out.push_str(eol);
    }
    if let Err(e) = std::fs::write(&resolved, out) {
        return error_response(
            "FILE_WRITE_ERROR",
            &format!("Cannot write file: {e}"),
            None,
            None,
        );
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
        // The source file had no trailing newline, so the rewrite must not
        // grow one (audit #128).
        assert_eq!(text, "a\nX\nY\nZ\nd");
        fs::remove_dir_all(f.parent().unwrap()).ok();
    }

    #[test]
    fn replace_lines_preserves_crlf_and_trailing_newline_state() {
        // CRLF + trailing newline: both preserved (audit #128 — the old
        // join("\n") converted the whole file to LF).
        let f = tmp("crlf");
        fs::write(&f, "a\r\nb\r\nc\r\n").unwrap();
        file_replace_lines(f.to_str().unwrap(), 2, 2, "X", false);
        assert_eq!(fs::read_to_string(&f).unwrap(), "a\r\nX\r\nc\r\n");
        fs::remove_dir_all(f.parent().unwrap()).ok();

        // LF with trailing newline: preserved too.
        let f = tmp("lf-trail");
        fs::write(&f, "a\nb\n").unwrap();
        file_replace_lines(f.to_str().unwrap(), 1, 1, "X", false);
        assert_eq!(fs::read_to_string(&f).unwrap(), "X\nb\n");
        fs::remove_dir_all(f.parent().unwrap()).ok();
    }

    #[test]
    fn read_with_extreme_line_numbers_does_not_wrap_or_panic() {
        // Audit #123: `end_line = -1` wrapped through `as usize` to ~2^64
        // (silently reading to EOF); `start_line = i64::MAX` overflowed the
        // window add. Both must behave sanely now.
        let f = tmp("extreme");
        file_write(f.to_str().unwrap(), "one\ntwo\nthree", false);

        // end_line < 1 is not a line number: treated as "no end given",
        // i.e. the default window from start_line.
        let s = file_read(f.to_str().unwrap(), Some(2), Some(-1), None);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["metadata"]["start_line"], 2);
        assert_eq!(v["metadata"]["end_line"], 3);
        assert!(v["data"].as_str().unwrap().contains("2: two"));

        // A start past EOF yields an empty page, not an overflow.
        let s = file_read(f.to_str().unwrap(), Some(i64::MAX), None, None);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["data"].as_str().unwrap(), "");
        assert_eq!(v["metadata"]["total_lines"], 3);

        let s = file_read(f.to_str().unwrap(), Some(1), Some(i64::MAX), None);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["metadata"]["end_line"], 3);

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

    /// A query searches the whole file, so a `start_line`/`end_line` sent
    /// alongside it does not apply. That was always true; it was never said,
    /// so a caller who passed both had no way to tell the range had been
    /// dropped rather than simply matching nothing.
    #[test]
    fn a_query_declares_that_it_ignored_the_line_range() {
        let dir = std::env::temp_dir().join(format!("kt-fs-range-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.txt");
        fs::write(
            &f,
            "alpha
beta
gamma
delta
",
        )
        .unwrap();

        let out = file_read(f.to_str().unwrap(), Some(1), Some(2), Some("delta"));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["metadata"]["line_range_ignored"], serde_json::json!(true));
        // And it really did search past the range it was given.
        assert!(v["data"].as_str().unwrap().contains("delta"));

        // No range passed, no note.
        let out = file_read(f.to_str().unwrap(), None, None, Some("delta"));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["metadata"].get("line_range_ignored").is_none());

        fs::remove_dir_all(&dir).ok();
    }
}
