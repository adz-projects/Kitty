//! Every document reader must fit the daemon's 100 KB tool-result cap *and*
//! keep the metadata that says how to continue.
//!
//! The daemon truncates each tool result at `MAX_TOOL_OUTPUT_BYTES`
//! (`BigTinyV2/daemon/src/mcp/tools.rs`) and serializes nothing back in: the
//! envelope puts `data` before `message` and `metadata`, so an oversized read
//! lost `document_id`, `has_more` and `next_offset` and left the model a torn
//! JSON fragment. These read ordinary documents with *default* arguments —
//! exactly what a model does first — and assert the whole response survives
//! the cut with a working continuation.

use std::path::{Path, PathBuf};
use std::process::Command;

use kitty_tools::server::{DocReadChunkRequest, DocSearchRequest, KittyToolsServer};
use kitty_tools::tools::fs::file_read;
use kitty_tools::tools::pdf::pdf_read_text;
use rmcp::handler::server::wrapper::Parameters;
use serde_json::Value;

/// The daemon's cap (`MAX_TOOL_OUTPUT_BYTES`).
const DAEMON_CAP: usize = 100 * 1024;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kt-budget-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Parses `out` as the model will see it, failing loudly if it would not
/// have survived the daemon's cut whole.
fn within_cap(out: &str) -> Value {
    assert!(
        out.len() <= DAEMON_CAP,
        "response is {} bytes; the daemon would cut it at {DAEMON_CAP}",
        out.len()
    );
    serde_json::from_str(out).unwrap_or_else(|e| panic!("not JSON: {e}"))
}

/// A 200-page PDF of ordinary prose, ~3.5 KB a page: the default read (100
/// pages) is ~350 KB, more than three times the cap.
fn long_pdf(path: &Path) {
    let status = Command::new("python")
        .arg("-c")
        .arg(format!(
            r#"
import fitz
doc = fitz.open()
line = "The committee reviewed the quarterly figures and noted steady progress. "
for n in range(200):
    page = doc.new_page()
    body = "\n".join(f"Page {{n + 1}} line {{i}}: " + line for i in range(48))
    page.insert_textbox(fitz.Rect(20, 20, 580, 820), body, fontsize=6)
doc.save(r"{}")
"#,
            path.display()
        ))
        .status()
        .expect("failed to run python");
    assert!(status.success());
}

#[tokio::test]
async fn a_long_pdf_read_with_defaults_fits_and_continues_to_the_end() {
    let dir = scratch("pdf");
    let path = dir.join("long.pdf");
    long_pdf(&path);
    let p = path.to_str().unwrap();

    let v = within_cap(&pdf_read_text(p, None, None, None, 0));
    assert_eq!(v["status"], "success");
    let meta = &v["metadata"];
    let served = meta["end_page"].as_u64().unwrap();
    assert!(
        served > 5 && served < 100,
        "budget should stop well short of 100: {served}"
    );
    assert_eq!(meta["has_more"], true);
    assert_eq!(meta["next_offset"].as_u64(), Some(served));
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("response size limit"),
        "the model must be told why it stopped: {}",
        v["message"]
    );
    let id = meta["document_id"].as_str().unwrap().to_string();

    // Walk the rest by handle with default limits (200 units = 200 pages):
    // every response must fit, and together they must cover every page once.
    let server = KittyToolsServer::new();
    let mut offset = served;
    let mut pages_seen = served;
    let mut calls = 0;
    loop {
        calls += 1;
        assert!(calls < 50, "read loop did not terminate");
        let out = server
            .doc_read_chunk(Parameters(DocReadChunkRequest {
                document_id: id.clone(),
                offset: Some(offset as u32),
                limit: None,
            }))
            .await;
        let v = within_cap(&out);
        let got = v["data"].as_array().unwrap();
        assert!(!got.is_empty(), "a continuation must always make progress");
        let first = got[0].as_str().unwrap();
        assert!(
            first.starts_with(&format!("--- Page {} ---", offset + 1)),
            "chunk at offset {offset} began with {:?}",
            &first[..first.len().min(30)]
        );
        pages_seen += got.len() as u64;
        match v["metadata"]["next_offset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert_eq!(pages_seen, 200, "every page exactly once");

    // A keyword that hits every page: fifty whole pages would be ~175 KB.
    let out = server
        .doc_search(Parameters(DocSearchRequest {
            document_id: id,
            query: "committee".into(),
            offset: None,
        }))
        .await;
    let v = within_cap(&out);
    assert!(
        v["metadata"]["next_offset"].as_u64().is_some(),
        "{}",
        v["metadata"]
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_explicit_huge_line_range_fits_and_continues() {
    let dir = scratch("lines");
    let path = dir.join("big.txt");
    let body: String = (1..=20_000)
        .map(|i| format!("row {i}: some ordinary content for this line\n"))
        .collect();
    std::fs::write(&path, body).unwrap();

    let v = within_cap(&file_read(
        path.to_str().unwrap(),
        Some(1),
        Some(20_000),
        None,
    ));
    let end = v["metadata"]["end_line"].as_u64().unwrap();
    assert!(end < 20_000);
    assert_eq!(v["metadata"]["has_more"], true);
    assert!(v["message"]
        .as_str()
        .unwrap()
        .contains(&format!("offset {end}")));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_single_enormous_line_is_cut_rather_than_overflowing() {
    let dir = scratch("minified");
    let path = dir.join("bundle.min.js");
    std::fs::write(&path, "x".repeat(2 * 1024 * 1024)).unwrap();

    let v = within_cap(&file_read(path.to_str().unwrap(), None, None, None));
    assert_eq!(v["truncated"], true);
    assert!(v["message"].as_str().unwrap().contains("cut short"));
    std::fs::remove_dir_all(&dir).ok();
}

/// PowerShell 5.1's `>` writes UTF-16LE with a BOM; that used to be
/// `FILE_READ_ERROR`.
#[test]
fn a_utf16_file_is_read_as_text() {
    let dir = scratch("utf16");
    let path = dir.join("out.txt");
    let mut bytes = vec![0xFF, 0xFE];
    for unit in "first line\r\nsecond — line\r\n".encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&path, bytes).unwrap();

    let v = within_cap(&file_read(path.to_str().unwrap(), None, None, None));
    assert_eq!(v["status"], "success", "{v}");
    let text = v["data"].as_str().unwrap();
    assert!(text.contains("1: first line"), "{text}");
    assert!(text.contains("2: second — line"), "{text}");
    assert_eq!(v["metadata"]["encoding"], "utf-16le");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_windows_1252_file_is_read_and_flagged_lossy() {
    let dir = scratch("cp1252");
    let path = dir.join("names.csv");
    // "café,naïve" in Windows-1252.
    std::fs::write(&path, b"caf\xe9,na\xefve\n").unwrap();

    let v = within_cap(&file_read(path.to_str().unwrap(), None, None, None));
    assert_eq!(v["status"], "success", "{v}");
    assert!(v["data"].as_str().unwrap().contains("café,naïve"));
    assert_eq!(v["metadata"]["lossy"], true);
    std::fs::remove_dir_all(&dir).ok();
}

/// `lean_file_read` takes no offset, so a query with more than one page of
/// matches must hand over to `lean_doc_search` rather than strand the rest.
#[test]
fn a_file_query_with_many_matches_points_at_the_continuation() {
    let dir = scratch("fquery");
    let path = dir.join("log.txt");
    let body: String = (1..=300)
        .map(|i| format!("event {i} warning raised\n"))
        .collect();
    std::fs::write(&path, body).unwrap();

    let v = within_cap(&file_read(
        path.to_str().unwrap(),
        None,
        None,
        Some("warning"),
    ));
    assert_eq!(v["metadata"]["total_matches"], 300);
    assert_eq!(v["metadata"]["next_offset"], 50);
    assert!(v["message"].as_str().unwrap().contains("lean_doc_search"));
    std::fs::remove_dir_all(&dir).ok();
}
