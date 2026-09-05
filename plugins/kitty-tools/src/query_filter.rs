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
    words_in_lowered(&s.to_lowercase())
}

fn words_in_lowered(lowered: &str) -> HashSet<String> {
    word_regex()
        .find_iter(lowered)
        .map(|m| m.as_str().to_string())
        .collect()
}

/// Stable score-descending indices of haystacks sharing at least one word
/// with the query. Ties keep document order (built in index order + stable
/// sort — do **not** sort ascending then reverse, which flips tie order).
///
/// Sound substring pre-filter: sharing a word implies containing it as a
/// substring, so a haystack containing none of the query words as substrings
/// scores 0 without paying for the regex + `HashSet` build. No false
/// negatives — every skipped item would have scored 0 anyway.
fn rank(haystacks: &[String], query_words: &HashSet<String>) -> Vec<usize> {
    let mut scored: Vec<(usize, usize)> = Vec::new();
    for (idx, item) in haystacks.iter().enumerate() {
        let lowered = item.to_lowercase();
        if !query_words.iter().any(|w| lowered.contains(w.as_str())) {
            continue;
        }
        let score = words_in_lowered(&lowered)
            .intersection(query_words)
            .count();
        if score > 0 {
            scored.push((score, idx));
        }
    }

    // Stable descending sort by score, ties keep document order. `Reverse`
    // only flips the comparator (not the whole vec), so — unlike
    // `sort_by_key(...); .reverse()`, which WOULD flip tie order — this
    // stays correct.
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(_, idx)| idx).collect()
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

    let ranked = rank(items, &query_words);

    if ranked.is_empty() {
        let (p, truncated, next_offset) = page(items, offset, max_results, items.len());
        return QueryFilterResult {
            items: p,
            truncated,
            total_matches: 0,
            next_offset,
            no_match: true,
        };
    }

    let total_matches = ranked.len();
    let ordered: Vec<String> = ranked
        .into_iter()
        .map(|idx| items[idx].clone())
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

/// Index-based sibling of `filter_by_query` for callers whose haystacks are
/// not the payload (e.g. Excel rows: plain-text haystacks for scoring, JSON
/// objects for output). Same ranking, same pagination — only the mapping
/// from rank to value differs, so results stay identical while the caller
/// clones just the returned page instead of every scanned row.
pub fn filter_indices(
    haystacks: &[String],
    query: &str,
    max_results: usize,
    offset: usize,
) -> (Vec<usize>, bool, usize, Option<usize>, bool) {
    // Punctuation-only query: verbatim page, mirroring `filter_by_query`'s
    // empty-word early return rather than reporting no match.
    let query_words = words_lower(query.trim());
    if query_words.is_empty() {
        let total = haystacks.len();
        let end = offset.saturating_add(max_results).min(total);
        let start = offset.min(total);
        let has_more = end < total;
        return (
            (start..end).collect(),
            has_more,
            total,
            has_more.then(|| end),
            false,
        );
    }
    let ranked = rank(haystacks, &query_words);
    if ranked.is_empty() {
        let total = haystacks.len();
        let end = offset.saturating_add(max_results).min(total);
        let start = offset.min(total);
        let idx: Vec<usize> = (start..end).collect();
        let has_more = end < total;
        return (
            idx,
            has_more,
            0,
            has_more.then(|| end),
            true,
        );
    }
    let total_matches = ranked.len();
    let end = offset.saturating_add(max_results).min(total_matches);
    let start = offset.min(total_matches);
    let idx: Vec<usize> = ranked[start..end].to_vec();
    let has_more = end < total_matches;
    (
        idx,
        has_more,
        total_matches,
        has_more.then(|| end),
        false,
    )
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
