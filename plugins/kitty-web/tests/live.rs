//! Live network tests — **all `#[ignore]`d**, so `cargo test` stays hermetic
//! and offline-safe. Run them deliberately:
//!
//! ```text
//! cargo test --test live -- --ignored --nocapture
//! ```
//!
//! These exist because the unit tests in `search.rs`/`scrape.rs` verify the
//! parsers against *fixtures we wrote*, which proves the parsing logic but
//! not that the fixtures still resemble reality. DuckDuckGo's and Bing's HTML
//! are unversioned scraping targets — both parsers are written to degrade
//! rather than fail when they drift, and this is how that drift gets noticed.
//!
//! Drift and a bot challenge have to be told apart, or these tests just go red
//! whenever an engine is throttling. The discriminator is
//! `metadata.engines`: an engine that reports `"ok"` and yet contributed zero
//! results has **drifted** and fails the test; one that reports `"blocked"`
//! was refused, which is an environment fact, not a regression.

use std::collections::HashSet;

use serde_json::Value;

/// Asserts that every engine claiming `"ok"` actually contributed results, and
/// reports the ones that were challenged. Returns the engines that answered.
fn assert_no_parser_drift(v: &Value) -> HashSet<String> {
    let engines = v["metadata"]["engines"]
        .as_object()
        .expect("metadata.engines present");
    let contributed: HashSet<String> = v["data"]
        .as_array()
        .expect("data is an array")
        .iter()
        .filter_map(|r| r["engine"].as_str().map(str::to_string))
        .collect();

    for (engine, status) in engines {
        match status.as_str().unwrap_or("") {
            "ok" => assert!(
                contributed.contains(engine),
                "{engine} reported ok but contributed no results — its parser has                  drifted against the live markup"
            ),
            "blocked" => eprintln!("note: {engine} was challenged (environmental, not a bug)"),
            other => eprintln!("note: {engine} unavailable: {other}"),
        }
    }
    contributed
}

#[tokio::test]
#[ignore = "hits the live network"]
async fn live_keyfree_search_returns_usable_results() {
    // No BRAVE_API_KEY in the default dev environment, so `normal` mode
    // exercises the key-free co-equal pair (DuckDuckGo + Bing).
    let out = kitty_web::search::web_search("rust programming language", 5, "en", None, "US").await;
    let v: Value = serde_json::from_str(&out).expect("valid JSON envelope");
    eprintln!("{}", serde_json::to_string_pretty(&v).unwrap());

    assert_eq!(v["status"], "success", "search failed: {v}");
    let results = v["data"].as_array().expect("data is an array");
    assert!(!results.is_empty(), "no results parsed out of live HTML");

    for r in results {
        let url = r["url"].as_str().unwrap_or("");
        assert!(
            url.starts_with("http"),
            "unwrapped url expected, got {url:?}"
        );
        assert!(
            !url.contains("duckduckgo.com/l/") && !url.contains("bing.com/ck/a"),
            "redirect wrapper leaked into result url: {url}"
        );
        assert!(
            !r["title"].as_str().unwrap_or("").is_empty(),
            "empty title in {r}"
        );
        assert!(
            r.get("snippet_full").is_none(),
            "snippet_full leaked inline"
        );
    }

    assert_no_parser_drift(&v);

    // Every search offloads, so read_chunk must resolve against this id.
    let search_id = v["metadata"]["search_id"]
        .as_str()
        .expect("search_id present");
    let chunk = kitty_web::search::web_search_read_chunk(search_id, &[1]);
    let cv: Value = serde_json::from_str(&chunk).unwrap();
    assert_eq!(cv["status"], "success", "read_chunk failed: {cv}");
    assert!(!cv["data"].as_array().unwrap().is_empty());
}

/// Bing's job is to answer when DuckDuckGo is being challenged, so it gets its
/// own drift check rather than hiding inside the merged result set.
#[tokio::test]
#[ignore = "hits the live network"]
async fn live_bing_search_returns_usable_results() {
    // `count > 5` puts every key-free engine in play concurrently.
    let out =
        kitty_web::search::web_search("rust programming language", 10, "en", None, "US").await;
    let v: Value = serde_json::from_str(&out).expect("valid JSON envelope");
    eprintln!("{}", serde_json::to_string_pretty(&v).unwrap());

    assert_eq!(v["status"], "success", "search failed: {v}");
    let contributed = assert_no_parser_drift(&v);
    assert!(
        contributed.contains("bing"),
        "Bing contributed nothing; engines: {}",
        v["metadata"]["engines"]
    );

    for r in v["data"].as_array().unwrap() {
        if r["engine"] != "bing" {
            continue;
        }
        let url = r["url"].as_str().unwrap_or("");
        assert!(
            url.starts_with("http") && !url.contains("bing.com/ck/a"),
            "Bing redirect wrapper was not decoded: {url}"
        );
        assert!(
            !r["title"].as_str().unwrap_or("").is_empty(),
            "empty title in {r}"
        );
    }
}

/// The regression this whole change exists for.
///
/// Three parallel `researcher` specialists burst concurrent searches through
/// one shared `kitty-web` process. Before the fix, most of those came back as
/// a DuckDuckGo challenge reported as `NO_RESULTS`, the specialists reworded
/// and retried, and all three burned their 300s budget. Every one of these
/// must now come back with results.
#[tokio::test]
#[ignore = "hits the live network"]
async fn live_parallel_searches_all_return_results() {
    let queries = [
        "rust programming language",
        "kubernetes operator pattern",
        "postgres index types",
        "llm agent frameworks",
        "golang error handling",
        "sqlite wal mode",
    ];

    let mut handles = Vec::new();
    for q in queries {
        handles.push(tokio::spawn(async move {
            let out = kitty_web::search::web_search(q, 5, "en", None, "US").await;
            (
                q,
                serde_json::from_str::<Value>(&out).expect("valid JSON envelope"),
            )
        }));
    }

    let mut failures = Vec::new();
    for h in handles {
        let (q, v) = h.await.unwrap();
        let engines = v["metadata"]["engines"].to_string();
        let count = v["data"].as_array().map(|a| a.len()).unwrap_or(0);
        eprintln!(
            "{q:>34}: status={} results={count} engines={engines}",
            v["status"]
        );
        if v["status"] != "success" || count == 0 {
            failures.push(format!("{q}: {} / engines={engines}", v["error"]));
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} parallel searches came back empty:
  {}",
        failures.len(),
        queries.len(),
        failures.join(
            "
  "
        )
    );
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
    eprintln!(
        "--- first 600 chars ---\n{}",
        body.chars().take(600).collect::<String>()
    );
    assert!(body.len() > 500, "suspiciously short extraction: {body}");
    assert!(
        body.to_lowercase().contains("webassembly"),
        "extraction missed the subject entirely"
    );
    // Boilerplate that must not survive extraction.
    assert!(!body.contains("<script"), "raw script tag leaked");
    assert!(!body.contains("Jump to content"), "nav chrome leaked");

    assert!(
        v["metadata"]["title"].as_str().is_some(),
        "no title metadata"
    );
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
