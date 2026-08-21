//! In-tool keyword RAG helper — Rust port of `lean_mcp.py`'s
//! `_filter_by_query`, with the Track E fixes applied (offset-based
//! continuation; no fabricated "no direct matches" string spliced into the
//! returned data — callers surface that as a `message` instead).

use regex::Regex;
use std::collections::HashSet;
use std::sync::OnceLock;

fn word_regex() -> &'static Regex {
    // `\w` is Unicode-aware by default in the `regex` crate (matching
    // Python's `re` module default) — do not add `(?-u)` or otherwise
    // disable Unicode support, that would silently make this ASCII-only.
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\w+").unwrap())
}

fn words_lower(s: &str) -> HashSet<String> {
    word_regex()
        .find_iter(&s.to_lowercase())
        .map(|m| m.as_str().to_string())
        .collect()
}

pub struct QueryFilterResult {
    pub items: Vec<String>,
    pub truncated: bool,
    pub total_matches: usize,
    pub next_offset: Option<usize>,
    /// True when a query was given but nothing scored > 0 — the caller
    /// should surface this as a message, never fabricate a line inside the
    /// returned data.
    pub no_match: bool,
}

fn page(
    items: &[String],
    offset: usize,
    max_results: usize,
    total: usize,
) -> (Vec<String>, bool, Option<usize>) {
    let page: Vec<String> = items
        .iter()
        .skip(offset)
        .take(max_results)
        .cloned()
        .collect();
    let has_more = offset + page.len() < total;
    let next_offset = if has_more {
        Some(offset + page.len())
    } else {
        None
    };
    (page, has_more, next_offset)
}

/// Filters lines/paragraphs/rows by keyword match score.
///
/// Stable descending sort — ties keep document order. `scored` is built in
/// index order and Rust's `sort_by` is stable, so sorting once by score
/// descending preserves original order among ties. Do **not** sort ascending
/// then reverse — that flips tie order (see the base plan's warning on this
/// exact trap).
pub fn filter_by_query(
    items: &[String],
    query: Option<&str>,
    max_results: usize,
    offset: usize,
) -> QueryFilterResult {
    let query = query.map(str::trim).filter(|q| !q.is_empty());

    let Some(query) = query else {
        let (p, truncated, next_offset) = page(items, offset, max_results, items.len());
        return QueryFilterResult {
            items: p,
            truncated,
            total_matches: items.len(),
            next_offset,
            no_match: false,
        };
    };

    let query_words = words_lower(query);
    if query_words.is_empty() {
        let (p, truncated, next_offset) = page(items, offset, max_results, items.len());
        return QueryFilterResult {
            items: p,
            truncated,
            total_matches: items.len(),
            next_offset,
            no_match: false,
        };
    }

    let mut scored: Vec<(usize, usize, &String)> = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let item_words = words_lower(item);
        let score = query_words.intersection(&item_words).count();
        if score > 0 {
            scored.push((score, idx, item));
        }
    }

    if scored.is_empty() {
        let (p, truncated, next_offset) = page(items, offset, max_results, items.len());
        return QueryFilterResult {
            items: p,
            truncated,
            total_matches: 0,
            next_offset,
            no_match: true,
        };
    }

    // Stable descending sort by score, ties keep document order. `Reverse`
    // only flips the comparator (not the whole vec), so — unlike
    // `sort_by_key(...); .reverse()`, which WOULD flip tie order — this
    // stays correct.
    scored.sort_by_key(|(score, _, _)| std::cmp::Reverse(*score));
    let total_matches = scored.len();
    let ordered: Vec<String> = scored
        .into_iter()
        .map(|(_, _, item)| item.clone())
        .collect();
    let (p, truncated, next_offset) = page(&ordered, offset, max_results, total_matches);
    QueryFilterResult {
        items: p,
        truncated,
        total_matches,
        next_offset,
        no_match: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_match_reports_no_match_without_fabricating_data() {
        let items = v(&["apple", "banana", "cherry"]);
        let r = filter_by_query(&items, Some("zzz-nonexistent"), 50, 0);
        assert!(r.no_match);
        assert_eq!(r.items, items);
        assert_eq!(r.total_matches, 0);
    }

    #[test]
    fn stable_sort_keeps_tie_order() {
        let items = v(&["cat dog", "dog cat", "dog only"]);
        let r = filter_by_query(&items, Some("cat dog"), 50, 0);
        assert_eq!(r.items[0], "cat dog");
        assert_eq!(r.items[1], "dog cat");
        assert_eq!(r.items[2], "dog only");
    }

    #[test]
    fn offset_and_next_offset_paginate_correctly() {
        let items: Vec<String> = (0..10).map(|i| format!("apple item {i}")).collect();
        let first = filter_by_query(&items, Some("apple"), 4, 0);
        assert_eq!(first.items.len(), 4);
        assert!(first.truncated);
        assert_eq!(first.next_offset, Some(4));

        let second = filter_by_query(&items, Some("apple"), 4, first.next_offset.unwrap());
        assert_eq!(second.items, items[4..8]);
        assert_eq!(second.next_offset, Some(8));
    }

    #[test]
    fn no_query_pages_items_verbatim() {
        let items: Vec<String> = (0..5).map(|i| format!("line {i}")).collect();
        let r = filter_by_query(&items, None, 3, 0);
        assert_eq!(r.items, items[0..3]);
        assert!(r.truncated);
        assert_eq!(r.total_matches, 5);
    }
}
