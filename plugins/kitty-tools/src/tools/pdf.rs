//! PDF read tools — Rust port of `kitty_docs_web.py`'s `lean_pdf_read_text`
//! and `lean_pdf_read_outline`, on `lopdf` (pure Rust) instead of PyMuPDF.
//!
//! Tool names, JSON envelope, error codes and pagination/query contracts are
//! kept byte-identical to the Python original. The one accepted divergence:
//! lopdf does plain per-page text extraction with no PyMuPDF markdown/layout
//! pass (`get_text`), so text runs/columns may come back in a different order
//! than PyMuPDF produced. Documented in docs/VERSIONS.md, same as the
//! DDG-scrape/htmd substitutions in kitty-web.

use std::path::Path;

use serde_json::{json, Value};

use crate::envelope::{error_response, success_response};
use crate::query_filter::filter_by_query;
use crate::paths::{path_within_home, resolve};

/// Hard cap on pages extracted in one call when no `end_page` is given — a
/// 10,000-page PDF must not balloon the payload. Still large enough for any
/// plausible interactive document.
const PDF_MAX_PAGES: u32 = 100;
/// Per-page text cap — an attack/malformed page can otherwise yield an
/// effectively unbounded extracted string.
const PDF_MAX_PAGE_CHARS: usize = 50_000;
/// Hard cap on the PDF file size, checked before `lopdf::Document::load`
/// (audit #120): `load` reads the whole file into memory, so a giant file
/// must be rejected at the door. Same pattern as `fs.rs`'s `MAX_FILE_BYTES`.
const PDF_MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

fn open(path: &Path) -> Result<lopdf::Document, lopdf::Error> {
    lopdf::Document::load(path)
}

/// True when the file's metadata size is past `PDF_MAX_FILE_BYTES` — the
/// check that keeps giant PDFs from being loaded into memory at all.
fn file_size_exceeds(resolved: &Path) -> bool {
    std::fs::metadata(resolved).map(|m| m.len() > PDF_MAX_FILE_BYTES).unwrap_or(false)
}

fn too_large(resolved: &Path) -> String {
    error_response(
        "PDF_TOO_LARGE",
        &format!("File is larger than the {} byte read limit", PDF_MAX_FILE_BYTES),
        Some(&resolved.to_string_lossy()),
        Some("Split the PDF, or read a page range from a smaller copy."),
    )
}

/// Home boundary shared by both PDF tools — defense-in-depth before any
/// filesystem access (the daemon is the primary gate).
fn outside_home(resolved: &Path) -> Option<String> {
    if path_within_home(resolved) {
        None
    } else {
        Some(error_response(
            "PATH_OUTSIDE_HOME",
            "Path is outside the HOME directory",
            Some(&resolved.to_string_lossy()),
            Some("Only paths inside your home directory can be accessed."),
        ))
    }
}

/// Truncates `s` to at most `max_chars` characters, appending a `…` marker
/// so truncation is visible in the payload.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        return s.to_string();
    }
    let kept: String = chars[..max_chars].iter().collect();
    format!("{kept}…")
}

pub fn pdf_read_text(
    path: &str,
    start_page: Option<u32>,
    end_page: Option<u32>,
    query: Option<&str>,
    offset: usize,
) -> String {
    let resolved = resolve(path);
    if let Some(err) = outside_home(&resolved) {
        return err;
    }
    if !resolved.exists() {
        return error_response("PDF_NOT_FOUND", "PDF does not exist", Some(&resolved.to_string_lossy()), None);
    }
    if file_size_exceeds(&resolved) {
        return too_large(&resolved);
    }

    let doc = match open(&resolved) {
        Ok(d) => d,
        Err(e) => {
            return error_response("PDF_CORRUPT", &format!("Cannot parse PDF: {e}"), Some(&resolved.to_string_lossy()), None);
        }
    };

    if doc.is_encrypted() {
        return error_response("PDF_ENCRYPTED", "PDF is password protected", Some(&resolved.to_string_lossy()), None);
    }

    let total_pages = doc.get_pages().len() as u32;
    let s_page = start_page.unwrap_or(1).max(1);
    // Both the caller's `end_page` and the hard cap bound the extraction; the
    // range is clamped to at most PDF_MAX_PAGES pages.
    let end_requested = end_page.map(|e| e.min(total_pages)).unwrap_or(total_pages).max(s_page.saturating_sub(1));
    let capped_end = s_page.saturating_add(PDF_MAX_PAGES - 1).min(total_pages);
    let e_page = end_requested.min(capped_end);
    let mut truncated = end_requested > e_page;

    let mut extracted_pages: Vec<String> = Vec::new();
    if s_page <= e_page {
        for pno in s_page..=e_page {
            let text = doc.extract_text(&[pno]).unwrap_or_default();
            let mut page_text = text.trim().to_string();
            if page_text.chars().count() > PDF_MAX_PAGE_CHARS {
                truncated = true;
                page_text = truncate_chars(&page_text, PDF_MAX_PAGE_CHARS);
            }
            extracted_pages.push(format!("--- Page {pno} ---\n{page_text}"));
        }
    }

    if let Some(q) = query.filter(|q| !q.trim().is_empty()) {
        let result = filter_by_query(&extracted_pages, Some(q), 50, offset);
        let message = result
            .no_match
            .then(|| format!("No direct matches for query '{q}'. Showing top section."));
        let mut meta = serde_json::Map::new();
        meta.insert("start_page".into(), json!(s_page));
        meta.insert("end_page".into(), json!(e_page));
        meta.insert("filtered_by_query".into(), json!(q));
        meta.insert("total_matches".into(), json!(result.total_matches));
        meta.insert("offset".into(), json!(offset));
        if let Some(next) = result.next_offset {
            meta.insert("next_offset".into(), json!(next));
        }
        let any_truncated = truncated || result.truncated;
        return success_response(json!(result.items), message.as_deref(), any_truncated, Some(Value::Object(meta)));
    }

    let message = truncated.then(|| format!("Output truncated: limited to {PDF_MAX_PAGES} pages and {PDF_MAX_PAGE_CHARS} characters per page."));
    success_response(
        json!(extracted_pages),
        message.as_deref(),
        truncated,
        Some(json!({ "start_page": s_page, "end_page": e_page, "total_pages": total_pages })),
    )
}

pub fn pdf_read_outline(path: &str) -> String {
    let resolved = resolve(path);
    if let Some(err) = outside_home(&resolved) {
        return err;
    }
    if !resolved.exists() {
        return error_response("PDF_NOT_FOUND", "PDF does not exist", Some(&resolved.to_string_lossy()), None);
    }
    if file_size_exceeds(&resolved) {
        return too_large(&resolved);
    }

    let doc = match open(&resolved) {
        Ok(d) => d,
        Err(e) => {
            return error_response("PDF_CORRUPT", &format!("Cannot parse PDF: {e}"), Some(&resolved.to_string_lossy()), None);
        }
    };

    if doc.is_encrypted() {
        return error_response("PDF_ENCRYPTED", "PDF is password protected", Some(&resolved.to_string_lossy()), None);
    }

    // lopdf's get_toc already flattens the outline tree into
    // { level, title, page } — the same triple PyMuPDF's get_toc produced.
    let outline: Vec<Value> = match doc.get_toc() {
        Ok(toc) => toc
            .toc
            .into_iter()
            .map(|o| json!({ "level": o.level, "title": o.title, "page": o.page }))
            .collect(),
        Err(_) => Vec::new(),
    };

    success_response(json!(outline), None, false, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_reports_pdf_not_found() {
        // Inside home (temp dir) so the boundary passes through to the
        // not-found error path.
        let dir = std::env::temp_dir().join(format!("kt-pdf-missing-{}", std::process::id()));
        let missing = dir.join("does-not-exist.pdf");
        let out = pdf_read_text(missing.to_str().unwrap(), None, None, None, 0);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "error");
        assert_eq!(v["error_code"], "PDF_NOT_FOUND");
    }

    #[test]
    fn missing_outline_file_reports_pdf_not_found() {
        let dir = std::env::temp_dir().join(format!("kt-pdf-missing-o-{}", std::process::id()));
        let missing = dir.join("does-not-exist.pdf");
        let out = pdf_read_outline(missing.to_str().unwrap());
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error_code"], "PDF_NOT_FOUND");
    }

    #[test]
    fn outside_home_path_is_rejected() {
        #[cfg(windows)]
        let p = "C:\\Windows\\System32\\calc.exe";
        #[cfg(not(windows))]
        let p = "/etc/passwd";
        let out = pdf_read_text(p, None, None, None, 0);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error_code"], "PATH_OUTSIDE_HOME");
    }

    #[test]
    fn truncate_chars_limited_and_keeps_marker() {
        let s = "abcde";
        assert_eq!(truncate_chars(s, 3), "abc…");
        assert_eq!(truncate_chars(s, 5), "abcde");
    }

    #[test]
    fn oversized_pdf_is_rejected_before_loading() {
        // Audit #120: `Document::load` reads the whole file into memory; the
        // size gate must fire first (a 64 MiB write exercises the metadata
        // check without a valid PDF ever being parsed).
        let dir = std::env::temp_dir().join(format!("kt-pdf-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("big.pdf");
        std::fs::write(&f, vec![b'x'; (PDF_MAX_FILE_BYTES + 1) as usize]).unwrap();

        let out = pdf_read_text(f.to_str().unwrap(), None, None, None, 0);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error_code"], "PDF_TOO_LARGE");

        let out = pdf_read_outline(f.to_str().unwrap());
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error_code"], "PDF_TOO_LARGE");

        std::fs::remove_dir_all(&dir).ok();
    }
}
