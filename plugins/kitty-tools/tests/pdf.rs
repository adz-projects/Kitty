//! Conformance tests for the PDF tools against real `.pdf` fixtures generated
//! by `pymupdf` (the same library `kitty_docs_web.py` used) — mirrors
//! `word_conformance.rs`'s philosophy. A fixture PDF with two pages of text
//! and a two-entry outline is produced by Python and read back through the
//! Rust tools, and PyMuPDF itself is the oracle for the page count.

use std::path::{Path, PathBuf};
use std::process::Command;

use kitty_tools::tools::pdf::{pdf_read_outline, pdf_read_text};
use serde_json::Value;

fn run_python(script: &str) -> String {
    let output = Command::new("python")
        .arg("-c")
        .arg(script)
        .output()
        .expect("failed to run python");
    if !output.status.success() {
        panic!(
            "python script failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn tmp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kitty-tools-pdf-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn parse(json_str: &str) -> Value {
    serde_json::from_str(json_str).expect("tool output was not valid JSON")
}

fn make_fixture(path: &Path) {
    run_python(&format!(
        r#"
import fitz
doc = fitz.open()
doc.new_page().insert_text((72, 72), "Hello page one apple")
doc.new_page().insert_text((72, 72), "Hello page two banana")
doc.set_toc([[1, "Chapter 1", 1], [1, "Chapter 2", 2]])
doc.save(r"{}")
doc.close()
print("OK")
"#,
        path.display()
    ));
}

#[test]
fn pdf_read_text_and_outline_against_pymupdf_fixture() {
    let path = tmp_path("test.pdf");
    make_fixture(&path);
    let ps = path.to_string_lossy();

    let v = parse(&pdf_read_text(&ps, None, None, None, 0));
    assert_eq!(v["status"], "success", "{v}");
    assert_eq!(v["metadata"]["total_pages"], 2);
    assert_eq!(v["data"].as_array().unwrap().len(), 2);
    let p1 = v["data"][0].as_str().unwrap();
    assert!(p1.contains("--- Page 1 ---"), "got: {p1}");
    assert!(p1.contains("apple"), "got: {p1}");
    let p2 = v["data"][1].as_str().unwrap();
    assert!(p2.contains("banana"), "got: {p2}");

    let o = parse(&pdf_read_outline(&ps));
    assert_eq!(o["status"], "success", "{o}");
    let outline = o["data"].as_array().unwrap();
    assert_eq!(outline.len(), 2);
    assert_eq!(outline[0]["level"], 1);
    assert_eq!(outline[0]["title"], "Chapter 1");
    assert_eq!(outline[0]["page"], 1);
    assert_eq!(outline[1]["title"], "Chapter 2");
}

#[test]
fn pdf_read_text_query_filters_pages() {
    let path = tmp_path("query.pdf");
    make_fixture(&path);
    let ps = path.to_string_lossy();

    let v = parse(&pdf_read_text(&ps, None, None, Some("banana"), 0));
    assert_eq!(v["status"], "success", "{v}");
    assert_eq!(v["metadata"]["total_matches"], 1);
    let data = v["data"].as_array().unwrap();
    assert!(data[0].as_str().unwrap().contains("banana"));
    // The non-matching page must not be present.
    assert!(!data[0].as_str().unwrap().contains("apple"));
}

#[test]
fn pdf_read_text_page_range() {
    let path = tmp_path("range.pdf");
    make_fixture(&path);
    let ps = path.to_string_lossy();

    let v = parse(&pdf_read_text(&ps, Some(2), Some(2), None, 0));
    assert_eq!(v["metadata"]["start_page"], 2);
    assert_eq!(v["metadata"]["end_page"], 2);
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
}

#[test]
fn pdf_read_text_not_found() {
    let dir = std::env::temp_dir().join(format!(
        "kitty-tools-pdf-missing-{}",
        std::process::id()
    ));
    let missing = dir.join("does-not-exist.pdf");
    let v = parse(&pdf_read_text(missing.to_str().unwrap(), None, None, None, 0));
    assert_eq!(v["error_code"], "PDF_NOT_FOUND");
}

#[test]
fn pdf_read_text_page_cap_prevents_giant_payloads() {
    // A 105-page PDF with no end_page must be capped at PDF_MAX_PAGES and
    // flagged `truncated` rather than returning every page.
    let path = tmp_path("many_pages.pdf");
    run_python(&format!(
        r#"
import fitz
doc = fitz.open()
for i in range(105):
    page = doc.new_page()
    page.insert_text((72, 72), f"Page number {{i}} apple")
doc.save(r"{}")
doc.close()
print("OK")
"#,
        path.display()
    ));

    let v = parse(&pdf_read_text(&path.to_string_lossy(), None, None, None, 0));
    assert_eq!(v["status"], "success", "{v}");
    assert_eq!(v["truncated"], true);
    let data = v["data"].as_array().unwrap();
    assert_eq!(data.len(), 100, "page cap must stop at 100 pages");
    assert!(v["metadata"]["end_page"].as_u64().unwrap() <= 100);
    assert!(v["metadata"]["total_pages"].as_u64().unwrap() == 105);
}

/// The point of the extract-once cache, on the exact case that motivated it:
/// a PDF longer than one response can carry. The first call caps at
/// `PDF_MAX_PAGES` as it always did, but the *whole* document is already
/// extracted behind a `document_id`, so pages past the cap come back without
/// the file being parsed a second time — and both PDF tools share the one
/// extraction, so whichever the model reaches for first pays for it.
#[test]
fn a_long_pdf_is_parsed_once_and_read_past_the_page_cap_by_handle() {
    use kitty_tools::server::{DocReadChunkRequest, KittyToolsServer};
    use rmcp::handler::server::wrapper::Parameters;

    let path = tmp_path("handle.pdf");
    run_python(&format!(
        r#"
import fitz
doc = fitz.open()
for i in range(105):
    page = doc.new_page()
    page.insert_text((72, 72), f"Page number {{i}} apple")
doc.set_toc([[1, "Only Chapter", 1]])
doc.save(r"{}")
doc.close()
print("OK")
"#,
        path.display()
    ));
    let ps = path.to_string_lossy();
    let server = KittyToolsServer::new();

    let first = parse(&pdf_read_text(&ps, None, None, None, 0));
    let id = first["metadata"]["document_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(first["data"].as_array().unwrap().len(), 100);
    assert_eq!(first["metadata"]["pages_available"], 105);
    // The outline rides along with the pages rather than needing its own parse.
    assert_eq!(first["metadata"]["outline"][0]["title"], "Only Chapter");

    // Same document, same handle — the outline tool reuses the extraction.
    let outline = parse(&pdf_read_outline(&ps));
    assert_eq!(
        outline["metadata"]["document_id"], id,
        "both PDF tools must share one extraction"
    );

    // Pages 101-105: unreachable in one response, one chunk call away.
    let tail = parse(&server.doc_read_chunk(Parameters(DocReadChunkRequest {
        document_id: id.clone(),
        offset: Some(100),
        limit: Some(200),
    })));
    assert_eq!(tail["status"], "success", "{tail}");
    assert_eq!(tail["metadata"]["unit"], "page");
    let pages = tail["data"].as_array().unwrap();
    assert_eq!(pages.len(), 5, "the last five pages must be reachable");
    assert!(pages[0].as_str().unwrap().contains("--- Page 101 ---"));
    assert!(pages[4].as_str().unwrap().contains("--- Page 105 ---"));
    assert_eq!(tail["metadata"]["has_more"], false);

    // An unchanged file keeps its handle across reads.
    let again = parse(&pdf_read_text(&ps, None, None, None, 0));
    assert_eq!(again["metadata"]["document_id"], id);
}
