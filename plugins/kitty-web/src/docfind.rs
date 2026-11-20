//! `lean_web_find_document` — find a named report or paper as a file.
//!
//! The usual way past a page that wants an email address before handing
//! over a report is that the same file is published somewhere else too: the
//! publisher's own CDN, a government or university repository, a press kit,
//! a conference site. This searches for the title as a file
//! (`"title" filetype:pdf`) and as a plain query, and ranks what comes back
//! by whether it is a direct file link and how much of the title it shares.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use crate::discover::words;
use crate::envelope::{error_response, success_response};
use crate::scrape::download_kind;
use crate::search::{normal_search, normalize_url_key, Locale, SearchItem};

/// Results returned, at most.
const MAX_RESULTS: usize = 8;
/// Results asked of each engine per query.
const PER_QUERY: usize = 5;
/// Share of the title's words a result must carry to be returned.
const MIN_MATCH: f64 = 0.4;

/// File types this tool will look for — the documents `lean_web_scrape`
/// can download for a reader.
const FILE_TYPES: &[&str] = &["pdf", "docx", "xlsx", "xls", "csv"];

/// One ranked result.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub item: SearchItem,
    /// The URL is itself a file of a downloadable type.
    pub file_type: Option<&'static str>,
    /// Share of the title's words found in the result's title, URL and
    /// snippet, 0–1.
    pub matched: f64,
}

pub async fn find_document(
    title: &str,
    publisher: Option<&str>,
    file_type: Option<&str>,
) -> String {
    let title = title.trim().trim_matches(['"', '\'']).trim();
    if title.is_empty() {
        return error_response(
            "EMPTY_QUERY",
            "No document title was given.",
            None,
            Some("Pass the report's or paper's title, as exactly as you know it."),
        );
    }
    let ft = file_type
        .map(|f| f.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| "pdf".to_string());
    if !FILE_TYPES.contains(&ft.as_str()) {
        return error_response(
            "INVALID_QUERY",
            &format!("Unsupported file_type '{ft}'."),
            None,
            Some("Use one of: pdf, docx, xlsx, xls, csv."),
        );
    }
    let title: String = title.chars().take(200).collect();
    let publisher = publisher
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| p.chars().take(80).collect::<String>());

    let exact = format!("\"{title}\" filetype:{ft}");
    let loose = match &publisher {
        Some(p) => format!("{title} {p} {ft}"),
        None => format!("{title} {ft}"),
    };
    let locale = Locale {
        lang: None,
        country: None,
    };
    let (a, b) = tokio::join!(
        normal_search(&exact, PER_QUERY, locale, None),
        normal_search(&loose, PER_QUERY, locale, None)
    );

    let mut engines: HashMap<String, String> = HashMap::new();
    let mut pool: Vec<SearchItem> = Vec::new();
    for outcome in [a, b] {
        match outcome {
            Ok((items, diag)) => {
                for (k, v) in diag {
                    // An engine that worked for either query worked.
                    let slot = engines.entry(k).or_insert_with(|| v.clone());
                    if v == "ok" {
                        *slot = v;
                    }
                }
                pool.extend(items);
            }
            Err(e) => {
                engines.insert("brave".into(), e.kind().into());
            }
        }
    }

    if pool.is_empty() && !engines.values().any(|s| s == "ok") {
        return error_response(
            "ALL_ENGINES_FAILED",
            "Every search engine failed; no search was actually performed.",
            Some(&serde_json::to_string(&engines).unwrap_or_default()),
            Some("A transient outage or rate limit. Retry once."),
        );
    }

    let ranked = rank(pool, &title, publisher.as_deref(), &ft);
    if ranked.is_empty() {
        return error_response(
            "NO_RESULTS",
            "No copy of that document was found.",
            None,
            Some(
                "Check the title's wording (a subtitle or year often differs), try without \
                 publisher, or search for coverage of the report with lean_web_search.",
            ),
        );
    }

    let direct = ranked.iter().filter(|f| f.file_type.is_some()).count();
    let data: Vec<Value> = ranked
        .iter()
        .map(|f| {
            json!({
                "url": f.item.url,
                "title": f.item.title,
                "domain": f.item.domain,
                "snippet": f.item.snippet,
                "file_type": f.file_type,
                "title_match": (f.matched * 100.0).round() / 100.0,
            })
        })
        .collect();
    let message = format!(
        "{direct} direct file link(s) among {} result(s). Download one with lean_web_scrape (it \
         saves the file to the cache for the reader tools). A copy found elsewhere can be a \
         different edition or a draft — check its title page and date before relying on it.",
        ranked.len()
    );
    success_response(
        Value::Array(data),
        Some(&message),
        false,
        Some(json!({
            "title": title,
            "publisher": publisher,
            "file_type": ft,
            "queries": [exact, loose],
            "engines": engines,
        })),
    )
}

/// Dedupe, score and order the pooled results: direct files of the wanted
/// type first, then other direct files, then pages; within each, by how
/// much of the title they carry (and the publisher, as a tie-break).
pub fn rank(pool: Vec<SearchItem>, title: &str, publisher: Option<&str>, ft: &str) -> Vec<Found> {
    let want = words(title);
    let publisher_words = publisher.map(words).unwrap_or_default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut found: Vec<(Found, usize)> = Vec::new();
    for item in pool {
        // One file served with different query strings (tracking, cache
        // busters) is one result; a page's query can matter, so only a
        // direct file link drops it.
        let path = item.url.split(['?', '#']).next().unwrap_or(&item.url);
        let is_file = download_kind(path, "").is_some();
        let dedup_key = normalize_url_key(if is_file { path } else { &item.url });
        if !seen.insert(dedup_key) {
            continue;
        }
        let haystack = words(&format!(
            "{} {} {}",
            item.title,
            url_words(&item.url),
            item.snippet
        ));
        let matched = if want.is_empty() {
            0.0
        } else {
            want.intersection(&haystack).count() as f64 / want.len() as f64
        };
        if matched < MIN_MATCH {
            continue;
        }
        let file_type = download_kind(path, "")
            .map(|k| k.ext)
            .filter(|e| FILE_TYPES.contains(e) || *e == "xlsm");
        let pub_hits = publisher_words.intersection(&haystack).count()
            + publisher_words
                .iter()
                .filter(|w| item.domain.to_lowercase().contains(w.as_str()))
                .count();
        found.push((
            Found {
                item,
                file_type,
                matched,
            },
            pub_hits,
        ));
    }
    let tier = |f: &Found| match f.file_type {
        Some(t) if t == ft => 0,
        Some(_) => 1,
        None => 2,
    };
    found.sort_by(|(a, pa), (b, pb)| {
        tier(a)
            .cmp(&tier(b))
            .then(
                b.matched
                    .partial_cmp(&a.matched)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(pb.cmp(pa))
    });
    found
        .into_iter()
        .map(|(f, _)| f)
        .take(MAX_RESULTS)
        .collect()
}

/// A URL's path as words (`/files/state-of-ports-2026.pdf` → state, ports,
/// 2026, pdf), so a file named after the report counts as a title match.
fn url_words(url: &str) -> String {
    let path = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split_once('/'))
        .map_or("", |(_, p)| p);
    path.replace(['-', '_', '/', '.', '+', '%'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, url: &str, snippet: &str) -> SearchItem {
        SearchItem {
            id: 0,
            title: title.into(),
            domain: url.split('/').nth(2).unwrap_or("").into(),
            url: url.into(),
            date: None,
            snippet: snippet.into(),
            snippet_full: String::new(),
            engine: "bing".into(),
        }
    }

    #[test]
    fn direct_files_rank_above_pages_and_weak_matches_are_dropped() {
        let pool = vec![
            item(
                "State of Ports 2026 | Harbour Council",
                "https://harbour.org/reports/state-of-ports-2026",
                "Download the State of Ports 2026 report",
            ),
            item(
                "PDF",
                "https://cdn.harbour.org/files/State-of-Ports-2026.pdf",
                "",
            ),
            item("Unrelated", "https://x.org/cats.pdf", "Cats and dogs"),
            item(
                "State of Ports 2026 data",
                "https://data.gov.example/ports-2026-state.xlsx",
                "",
            ),
        ];
        let ranked = rank(pool, "State of Ports 2026", Some("Harbour Council"), "pdf");
        let urls: Vec<&str> = ranked.iter().map(|f| f.item.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://cdn.harbour.org/files/State-of-Ports-2026.pdf",
                "https://data.gov.example/ports-2026-state.xlsx",
                "https://harbour.org/reports/state-of-ports-2026",
            ]
        );
        assert_eq!(ranked[0].file_type, Some("pdf"));
        assert_eq!(ranked[2].file_type, None);
    }

    #[test]
    fn duplicates_across_queries_collapse() {
        let a = item(
            "State of Ports",
            "https://www.h.org/r.pdf?utm_source=x",
            "state ports",
        );
        let b = item("State of Ports", "https://h.org/r.pdf", "state ports");
        let c = item(
            "State of Ports",
            "https://h.org/r.pdf?ashby_jid=1",
            "state ports",
        );
        assert_eq!(rank(vec![a, b, c], "State of Ports", None, "pdf").len(), 1);
        // Pages keep their query: it can select different content.
        let p1 = item("State of Ports", "https://h.org/view?id=1", "state ports");
        let p2 = item("State of Ports", "https://h.org/view?id=2", "state ports");
        assert_eq!(rank(vec![p1, p2], "State of Ports", None, "pdf").len(), 2);
    }

    #[tokio::test]
    async fn bad_input_is_refused_before_any_search() {
        let out = find_document("  ", None, None).await;
        assert!(out.contains("EMPTY_QUERY"));
        let out = find_document("Report", None, Some("exe")).await;
        assert!(out.contains("INVALID_QUERY"));
    }
}
