//! Live network tests — **all `#[ignore]`d**, so `cargo test` stays hermetic
//! and offline-safe. Run them deliberately:
//!
//! ```text
//! cargo test --test live -- --ignored --nocapture
//! ```
//!
//! These exist because the unit tests in `search.rs`/`scrape.rs` verify the
//! parsers against *fixtures we wrote*, which proves the parsing logic but
//! not that the fixtures still resemble reality. DuckDuckGo's HTML in
//! particular is an unversioned scraping target — `parse_ddg_html` is written
//! to degrade rather than fail when it drifts, and this is how that drift
//! gets noticed.

use serde_json::Value;

#[tokio::test]
#[ignore = "hits the live network"]
async fn live_duckduckgo_search_returns_usable_results() {
    // No BRAVE_API_KEY in the default dev environment, so `normal` mode
    // exercises the DuckDuckGo path specifically.
    let out = kitty_web::search::web_search("rust programming language", 5, "en", None, "US").await;
    let v: Value = serde_json::from_str(&out).expect("valid JSON envelope");
    eprintln!("{}", serde_json::to_string_pretty(&v).unwrap());

    assert_eq!(v["status"], "success", "search failed: {v}");
    let results = v["data"].as_array().expect("data is an array");
    assert!(!results.is_empty(), "no results parsed out of live HTML");

    for r in results {
        let url = r["url"].as_str().unwrap_or("");
        assert!(url.starts_with("http"), "unwrapped url expected, got {url:?}");
        assert!(
            !url.contains("duckduckgo.com/l/"),
            "redirect wrapper leaked into result url: {url}"
        );
        assert!(
            !r["title"].as_str().unwrap_or("").is_empty(),
            "empty title in {r}"
        );
        assert!(r.get("snippet_full").is_none(), "snippet_full leaked inline");
    }

    // Every search offloads, so read_chunk must resolve against this id.
    let search_id = v["metadata"]["search_id"].as_str().expect("search_id present");
    let chunk = kitty_web::search::web_search_read_chunk(search_id, &[1]);
    let cv: Value = serde_json::from_str(&chunk).unwrap();
    assert_eq!(cv["status"], "success", "read_chunk failed: {cv}");
    assert!(!cv["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
#[ignore = "hits the live network"]
async fn live_scrape_extracts_real_article_body() {
    let out = kitty_web::scrape::web_scrape(
        "https://en.wikipedia.org/wiki/WebAssembly",
        None,
        "markdown",
        0,
        Some(4000),
        false,
        false,
    )
    .await;
    let v: Value = serde_json::from_str(&out).expect("valid JSON envelope");
    assert_eq!(v["status"], "success", "scrape failed: {v}");

    let body = v["data"].as_str().expect("data is a string");
    eprintln!("--- first 600 chars ---\n{}", &body.chars().take(600).collect::<String>());
    assert!(body.len() > 500, "suspiciously short extraction: {body}");
    assert!(
        body.to_lowercase().contains("webassembly"),
        "extraction missed the subject entirely"
    );
    // Boilerplate that must not survive extraction.
    assert!(!body.contains("<script"), "raw script tag leaked");
    assert!(!body.contains("Jump to content"), "nav chrome leaked");

    assert!(v["metadata"]["title"].as_str().is_some(), "no title metadata");
    assert_eq!(v["metadata"]["content_type"], "text/html");
}

#[tokio::test]
#[ignore = "hits the live network"]
async fn live_scrape_reports_http_errors_structurally() {
    // Deliberately not httpbin.org: it is frequently slow enough to trip the
    // 30s timeout, which makes this assert `SCRAPE_TIMEOUT` instead — a
    // correct response to a timeout, but a flaky test. raw.githubusercontent
    // 404s fast and reliably.
    let out = kitty_web::scrape::web_scrape(
        "https://raw.githubusercontent.com/rust-lang/rust/master/definitely-not-a-file-xyz123.txt",
        None,
        "markdown",
        0,
        None,
        false,
        false,
    )
    .await;
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["status"], "error");
    assert_eq!(v["error_code"], "SCRAPE_HTTP_ERROR");
}
