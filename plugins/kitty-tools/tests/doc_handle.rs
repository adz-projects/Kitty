//! End-to-end tests for the extract-once document cache and the handle read
//! loop it exists to serve (`src/doc_store.rs`).
//!
//! These call the tool functions in-process, no MCP transport, the same way
//! `word_conformance.rs` does. What they are actually pinning is the claim the
//! feature is built on: a document is extracted **once**, and every subsequent
//! read of it — sequential or by keyword — is served from that one extraction
//! under a `document_id` that stays stable while the file does and changes the
//! moment it doesn't.

use std::path::PathBuf;

use kitty_tools::server::{DocReadChunkRequest, DocSearchRequest, KittyToolsServer};
use kitty_tools::tools::fs::file_read;
use rmcp::handler::server::wrapper::Parameters;
use serde_json::Value;

/// A scratch directory under the temp dir, which on every supported host sits
/// inside the user's home — the tools' path boundary rejects anything else, so
/// a fixture outside it would fail on containment before reaching the code
/// under test.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kt-dochandle-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn json(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|e| panic!("not JSON: {e}\n{s}"))
}

/// 500 numbered lines with one distinctive line planted deep enough that no
/// single default-sized window contains it.
fn long_file(dir: &std::path::Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let mut body = String::new();
    for i in 1..=500 {
        if i == 430 {
            body.push_str("the quarterly aardvark figures were unexpected\n");
        } else {
            body.push_str(&format!("filler line {i}\n"));
        }
    }
    std::fs::write(&path, body).unwrap();
    path
}

fn document_id_of(v: &Value) -> String {
    v["metadata"]["document_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no document_id in {v}"))
        .to_string()
}

#[test]
fn a_long_file_hands_back_a_stable_handle_and_the_rest_reads_from_cache() {
    let dir = scratch("loop");
    let path = long_file(&dir, "report.txt");
    let p = path.to_str().unwrap();
    let server = KittyToolsServer::new();

    // First read: the default window, plus a handle and an honest total.
    let first = json(&file_read(p, None, None, None));
    assert_eq!(first["status"], "success", "{first}");
    assert_eq!(first["metadata"]["total_lines"], 500);
    assert_eq!(first["metadata"]["has_more"], true);
    assert_eq!(first["metadata"]["unit"], "line");
    let id = document_id_of(&first);

    // The window really is only part of the file — this is the "truncated
    // chunks" complaint the handle exists to answer.
    let page_one = first["data"].as_str().unwrap();
    assert!(page_one.contains("1: filler line 1"));
    assert!(!page_one.contains("aardvark"));

    // Reading on by handle continues exactly where the window stopped, with
    // no path and no re-read.
    let end_line = first["metadata"]["end_line"].as_u64().unwrap() as u32;
    let next = json(&server.doc_read_chunk(Parameters(DocReadChunkRequest {
        document_id: id.clone(),
        offset: Some(end_line),
        limit: Some(50),
    })));
    assert_eq!(next["status"], "success", "{next}");
    assert_eq!(next["metadata"]["document_id"], id);
    let units = next["data"].as_array().unwrap();
    assert_eq!(units.len(), 50);
    assert_eq!(
        units[0].as_str().unwrap(),
        format!("{}: filler line {}", end_line + 1, end_line + 1),
        "the chunk must resume at the line after the window, with numbering intact"
    );
    assert_eq!(next["metadata"]["next_offset"], end_line as u64 + 50);

    std::fs::remove_dir_all(&dir).ok();
}

/// The other half of the loop: find the one line that matters without walking
/// the document a window at a time. `file_read`'s own `query` searches the
/// whole file too, but only by re-reading it by path; this searches the
/// extraction already in hand.
#[test]
fn search_by_handle_finds_content_outside_every_window() {
    let dir = scratch("search");
    let path = long_file(&dir, "notes.txt");
    let server = KittyToolsServer::new();

    let first = json(&file_read(path.to_str().unwrap(), None, None, None));
    let id = document_id_of(&first);

    let hit = json(&server.doc_search(Parameters(DocSearchRequest {
        document_id: id.clone(),
        query: "aardvark".to_string(),
        offset: None,
    })));
    assert_eq!(hit["status"], "success", "{hit}");
    assert_eq!(hit["metadata"]["document_id"], id);
    let items = hit["data"].as_array().unwrap();
    assert!(
        items
            .iter()
            .any(|i| i.as_str().unwrap_or_default().contains("aardvark")),
        "the planted line must be found: {hit}"
    );
    // Line numbering survives into search results, so a hit is addressable.
    assert!(items
        .iter()
        .any(|i| i.as_str().unwrap_or_default().starts_with("430: ")));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_empty_search_query_is_refused_rather_than_returning_the_document() {
    let dir = scratch("emptyq");
    let path = long_file(&dir, "q.txt");
    let server = KittyToolsServer::new();
    let id = document_id_of(&json(&file_read(path.to_str().unwrap(), None, None, None)));

    let out = json(&server.doc_search(Parameters(DocSearchRequest {
        document_id: id,
        query: "   ".to_string(),
        offset: None,
    })));
    assert_eq!(out["status"], "error", "{out}");
    assert_eq!(out["error_code"], "DOC_QUERY_EMPTY");

    std::fs::remove_dir_all(&dir).ok();
}

/// The cache's contract in one test: the same unchanged file keeps its handle,
/// and an edited one must not keep serving its previous contents.
#[test]
fn the_handle_is_stable_across_reads_and_moves_when_the_file_changes() {
    let dir = scratch("stable");
    let path = long_file(&dir, "stable.txt");
    let p = path.to_str().unwrap();

    let first = document_id_of(&json(&file_read(p, None, None, None)));
    let again = document_id_of(&json(&file_read(p, None, None, None)));
    assert_eq!(first, again, "an unchanged file must keep its handle");

    // Rewrite with different content; mtime moves, so the identity does too.
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(&path, "1: only one line now\n").unwrap();
    let after = json(&file_read(p, None, None, None));
    assert_ne!(
        document_id_of(&after),
        first,
        "an edited file must not reuse the previous handle"
    );
    assert_eq!(after["metadata"]["total_lines"], 1);

    std::fs::remove_dir_all(&dir).ok();
}

/// The store keeps only the newest 20 documents, so a handle going stale is an
/// expected outcome, not a caller error. It has to say so recoverably rather
/// than failing opaquely mid-read-loop — the exact failure `kitty-web`'s
/// `SEARCH_ID_NOT_FOUND` was fixed to stop producing silently.
#[test]
fn an_unknown_or_malformed_handle_fails_recoverably() {
    let server = KittyToolsServer::new();

    for bad in [
        "0123456789abcdef", // well-formed, simply not present
        "../../../../etc/passwd",
        "doc:stream",
        "not-hex",
        "",
    ] {
        let out = json(&server.doc_read_chunk(Parameters(DocReadChunkRequest {
            document_id: bad.to_string(),
            offset: None,
            limit: None,
        })));
        assert_eq!(out["status"], "error", "{bad}: {out}");
        assert_eq!(out["error_code"], "DOCUMENT_ID_NOT_FOUND", "{bad}");
        assert!(
            !out["hint"].as_str().unwrap_or_default().is_empty(),
            "{bad} must come with a way out"
        );
    }
}

/// Reading to the very end must terminate cleanly: the final window says
/// nothing follows, and an offset past the end is an empty answer rather than
/// an error, so a model walking `next_offset` can't get stuck.
#[test]
fn walking_to_the_end_terminates_instead_of_erroring() {
    let dir = scratch("end");
    let path = long_file(&dir, "end.txt");
    let server = KittyToolsServer::new();
    let id = document_id_of(&json(&file_read(path.to_str().unwrap(), None, None, None)));

    let last = json(&server.doc_read_chunk(Parameters(DocReadChunkRequest {
        document_id: id.clone(),
        offset: Some(450),
        limit: Some(200),
    })));
    assert_eq!(last["data"].as_array().unwrap().len(), 50);
    assert_eq!(last["metadata"]["has_more"], false);
    assert!(last["metadata"].get("next_offset").is_none());

    let past = json(&server.doc_read_chunk(Parameters(DocReadChunkRequest {
        document_id: id,
        offset: Some(9999),
        limit: Some(10),
    })));
    assert_eq!(past["status"], "success", "{past}");
    assert!(past["data"].as_array().unwrap().is_empty());
    assert_eq!(past["metadata"]["has_more"], false);

    std::fs::remove_dir_all(&dir).ok();
}
