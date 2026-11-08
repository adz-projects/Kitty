//! Paragraph-level intake (docs/MEMORY_OVERNIGHT.md §1 and §3): split a
//! document into paragraphs, drop the ones that are junk at no cost, and
//! group what the screen kept into chunks of at least two paragraphs and at
//! least 600 words.
//!
//! Pure and deterministic, so identical text always splits, filters and
//! groups identically (the precondition for the content-hash dedup).

use std::collections::HashMap;

/// Paragraphs shorter than this merge into the next one: a lone sentence or a
/// caption is too little for the screen to judge on its own.
pub const MIN_PARAGRAPH_CHARS: usize = 200;
/// Paragraphs longer than this split at a sentence boundary, so one screen
/// call never judges a wall of text as a single unit.
pub const MAX_PARAGRAPH_CHARS: usize = 1500;
/// A chunk closes once it holds at least this many words...
pub const CHUNK_MIN_WORDS: usize = 600;
/// ...and at least this many paragraphs, whichever is longer.
pub const CHUNK_MIN_PARAGRAPHS: usize = 2;

/// One paragraph, with the heading it sits under (its context for the
/// screen).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paragraph {
    pub heading: Option<String>,
    pub text: String,
}

/// Why the pre-filter dropped a paragraph. Logged in the screen log so the
/// rules can be audited against what the user would have kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefilterReason {
    TooShort,
    MostlyLinks,
    MostlyDigits,
    ShortLines,
    References,
    Repeated,
    Boilerplate,
}

impl PrefilterReason {
    pub fn as_str(self) -> &'static str {
        match self {
            PrefilterReason::TooShort => "too_short",
            PrefilterReason::MostlyLinks => "mostly_links",
            PrefilterReason::MostlyDigits => "mostly_digits",
            PrefilterReason::ShortLines => "short_lines",
            PrefilterReason::References => "references",
            PrefilterReason::Repeated => "repeated",
            PrefilterReason::Boilerplate => "boilerplate",
        }
    }
}

fn heading_text(line: &str) -> Option<String> {
    let t = line.trim_start();
    let hashes = t.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &t[hashes..];
    if !rest.starts_with(' ') {
        return None;
    }
    let h = rest.trim().trim_end_matches('#').trim();
    (!h.is_empty()).then(|| h.to_string())
}

fn is_fence(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

/// Raw blocks: runs of non-blank lines, with fenced code kept whole and each
/// markdown heading its own block.
fn raw_blocks(text: &str) -> Vec<(bool, String)> {
    // (is_heading, text)
    let mut out: Vec<(bool, String)> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut in_fence = false;
    let flush = |cur: &mut Vec<&str>, out: &mut Vec<(bool, String)>| {
        if !cur.is_empty() {
            let block = cur.join("\n").trim().to_string();
            if !block.is_empty() {
                out.push((false, block));
            }
            cur.clear();
        }
    };
    for line in text.lines() {
        if is_fence(line) {
            cur.push(line);
            if in_fence {
                in_fence = false;
                flush(&mut cur, &mut out);
            } else {
                in_fence = true;
            }
            continue;
        }
        if in_fence {
            cur.push(line);
            continue;
        }
        if line.trim().is_empty() {
            flush(&mut cur, &mut out);
            continue;
        }
        if let Some(h) = heading_text(line) {
            flush(&mut cur, &mut out);
            out.push((true, h));
            continue;
        }
        cur.push(line);
    }
    flush(&mut cur, &mut out);
    out
}

/// Split `text` at sentence ends so no piece exceeds `max` chars (a sentence
/// longer than `max` stays whole rather than being cut mid-word).
fn split_long(text: &str, max: usize) -> Vec<String> {
    if text.chars().count() <= max {
        return vec![text.to_string()];
    }
    let mut sentences: Vec<String> = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        cur.push(c);
        let end = matches!(c, '.' | '!' | '?')
            && chars.get(i + 1).is_none_or(|n| n.is_whitespace());
        if end {
            sentences.push(std::mem::take(&mut cur));
        }
    }
    if !cur.trim().is_empty() {
        sentences.push(cur);
    }
    let mut out: Vec<String> = Vec::new();
    let mut piece = String::new();
    for s in sentences {
        if !piece.is_empty() && piece.chars().count() + s.chars().count() > max {
            out.push(piece.trim().to_string());
            piece.clear();
        }
        piece.push_str(&s);
    }
    if !piece.trim().is_empty() {
        out.push(piece.trim().to_string());
    }
    out
}

fn is_short_line_block(block: &str) -> bool {
    let lines: Vec<&str> = block.lines().filter(|l| !l.trim().is_empty()).collect();
    lines.len() >= 3 && lines.iter().filter(|l| word_count(l) < 6).count() * 10 >= lines.len() * 6
}

fn is_table_or_list(block: &str) -> bool {
    let lines: Vec<&str> = block.lines().collect();
    lines.len() > 1
        && lines.iter().all(|l| {
            let t = l.trim_start();
            t.starts_with('|')
                || t.starts_with("- ")
                || t.starts_with("* ")
                || t.starts_with("+ ")
                || t.split_once(". ")
                    .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        })
}

/// Split a document into paragraphs: blank-line blocks, headings as context,
/// short blocks merged forward, long prose split at sentences. Tables and
/// lists stay whole.
pub fn split_paragraphs(text: &str) -> Vec<Paragraph> {
    let mut heading: Option<String> = None;
    let mut out: Vec<Paragraph> = Vec::new();
    let mut carry = String::new();
    let mut carry_heading: Option<String> = None;
    for (is_heading, block) in raw_blocks(text) {
        if is_heading {
            // A heading closes whatever short text is waiting: it belongs to
            // the previous section, not the next.
            if !carry.is_empty() {
                out.push(Paragraph {
                    heading: carry_heading.take(),
                    text: std::mem::take(&mut carry),
                });
            }
            heading = Some(block);
            continue;
        }
        // A block of short lines (a menu, a tag list) is never merged with
        // prose: merged, it would sink a good paragraph in the pre-filter.
        if is_short_line_block(&block) {
            if !carry.is_empty() {
                out.push(Paragraph {
                    heading: carry_heading.take(),
                    text: std::mem::take(&mut carry),
                });
            }
            out.push(Paragraph {
                heading: heading.clone(),
                text: block,
            });
            continue;
        }
        let joined = if carry.is_empty() {
            block
        } else {
            format!("{carry}\n\n{block}")
        };
        if carry.is_empty() {
            carry_heading = heading.clone();
        }
        carry.clear();
        if joined.chars().count() < MIN_PARAGRAPH_CHARS {
            carry = joined;
            continue;
        }
        let pieces = if is_table_or_list(&joined) {
            vec![joined]
        } else {
            split_long(&joined, MAX_PARAGRAPH_CHARS)
        };
        let h = carry_heading.take();
        for p in pieces {
            out.push(Paragraph {
                heading: h.clone(),
                text: p,
            });
        }
    }
    if !carry.is_empty() {
        // A short tail joins the paragraph before it when there is one in
        // the same section; otherwise it stands alone (and the pre-filter
        // decides).
        match out.last_mut() {
            Some(last) if last.heading == carry_heading => {
                last.text.push_str("\n\n");
                last.text.push_str(&carry);
            }
            _ => out.push(Paragraph {
                heading: carry_heading,
                text: carry,
            }),
        }
    }
    out
}

const BOILERPLATE: &[&str] = &[
    "all rights reserved",
    "cookie",
    "privacy policy",
    "terms of service",
    "terms of use",
    "subscribe to our newsletter",
    "sign up for our newsletter",
    "sign in to continue",
    "enable javascript",
    "share this article",
    "advertisement",
    "skip to main content",
];

fn word_count(s: &str) -> usize {
    s.split_whitespace().count()
}

fn link_chars(text: &str) -> usize {
    let mut n = 0usize;
    for word in text.split_whitespace() {
        if word.contains("http://") || word.contains("https://") || word.starts_with("www.") {
            n += word.chars().count();
        }
    }
    // Markdown link targets: `](...)`.
    let mut rest = text;
    while let Some(i) = rest.find("](") {
        let after = &rest[i + 2..];
        let end = after.find(')').unwrap_or(after.len());
        n += end + 3;
        rest = &after[end.min(after.len())..];
    }
    n
}

fn is_reference_line(line: &str) -> bool {
    let t = line.trim_start();
    let lower = t.to_lowercase();
    (t.starts_with('[') && t[1..].split(']').next().is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()) && !n.is_empty()))
        || t.starts_with('^')
        || lower.contains("doi:")
        || lower.contains("doi.org/")
        || lower.starts_with("retrieved ")
        || lower.contains("archived from the original")
        || lower.contains("isbn ")
}

/// Why `p` is junk, or `None` to send it to the screen. `seen` counts
/// paragraph texts across the document, for headers and footers that repeat.
pub fn prefilter(p: &Paragraph, seen: &HashMap<String, usize>) -> Option<PrefilterReason> {
    let text = p.text.trim();
    let chars = text.chars().count();
    let non_ws: usize = text.chars().filter(|c| !c.is_whitespace()).count();
    if non_ws == 0 || word_count(text) < 12 {
        return Some(PrefilterReason::TooShort);
    }
    if seen.get(text).copied().unwrap_or(0) > 1 {
        return Some(PrefilterReason::Repeated);
    }
    if link_chars(text) * 10 >= chars * 4 {
        return Some(PrefilterReason::MostlyLinks);
    }
    let digits = text.chars().filter(|c| c.is_ascii_digit()).count();
    if digits * 10 >= non_ws * 3 {
        return Some(PrefilterReason::MostlyDigits);
    }
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() >= 4 {
        let short = lines.iter().filter(|l| word_count(l) < 6).count();
        if short * 10 >= lines.len() * 6 {
            return Some(PrefilterReason::ShortLines);
        }
        let refs = lines.iter().filter(|l| is_reference_line(l)).count();
        if refs * 2 >= lines.len() {
            return Some(PrefilterReason::References);
        }
    } else if lines.len() == 1 && is_reference_line(text) {
        return Some(PrefilterReason::References);
    }
    let lower = text.to_lowercase();
    // Boilerplate only when the paragraph is short enough to be *about* it;
    // an article that mentions cookies in passing is still an article.
    if word_count(text) < 80 && BOILERPLATE.iter().any(|b| lower.contains(b)) {
        return Some(PrefilterReason::Boilerplate);
    }
    None
}

/// How many times each paragraph text occurs in the document.
pub fn occurrence_counts(paragraphs: &[Paragraph]) -> HashMap<String, usize> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    for p in paragraphs {
        *seen.entry(p.text.trim().to_string()).or_default() += 1;
    }
    seen
}

/// Group kept paragraphs, in document order, into chunks of at least
/// [`CHUNK_MIN_PARAGRAPHS`] paragraphs and at least [`CHUNK_MIN_WORDS`] words
/// — whichever is longer. A remainder that falls short joins the chunk before
/// it; a document with too little for one full chunk becomes one chunk.
pub fn group_chunks(kept: &[String]) -> Vec<String> {
    let mut chunks: Vec<Vec<&str>> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut words = 0usize;
    for p in kept {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        cur.push(p);
        words += word_count(p);
        if cur.len() >= CHUNK_MIN_PARAGRAPHS && words >= CHUNK_MIN_WORDS {
            chunks.push(std::mem::take(&mut cur));
            words = 0;
        }
    }
    if !cur.is_empty() {
        match chunks.last_mut() {
            Some(last) => last.extend(cur),
            None => chunks.push(cur),
        }
    }
    chunks.into_iter().map(|c| c.join("\n\n")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prose(words: usize, seed: &str) -> String {
        let mut s = String::new();
        for i in 0..words {
            s.push_str(seed);
            s.push_str(&format!("{} ", i % 7));
            if i % 15 == 14 {
                s.push_str("end. ");
            }
        }
        s.trim().to_string()
    }

    #[test]
    fn headings_become_context_not_paragraphs() {
        let text = format!("# Ports\n\n{}\n\n## Walls\n\n{}", prose(60, "harbor"), prose(60, "wall"));
        let ps = split_paragraphs(&text);
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0].heading.as_deref(), Some("Ports"));
        assert_eq!(ps[1].heading.as_deref(), Some("Walls"));
    }

    #[test]
    fn short_blocks_merge_forward_and_long_ones_split() {
        let text = format!("Short line.\n\n{}", prose(40, "word"));
        let ps = split_paragraphs(&text);
        assert_eq!(ps.len(), 1);
        assert!(ps[0].text.starts_with("Short line."));

        let long = prose(600, "long");
        let ps = split_paragraphs(&long);
        assert!(ps.len() > 1);
        assert!(ps.iter().all(|p| p.text.chars().count() <= MAX_PARAGRAPH_CHARS + 200));
    }

    #[test]
    fn tables_and_lists_stay_whole() {
        let table: String = (0..40)
            .map(|i| format!("| row {i} | value {i} | more text here |\n"))
            .collect();
        let ps = split_paragraphs(&table);
        assert_eq!(ps.len(), 1);
    }

    #[test]
    fn prefilter_drops_junk_and_keeps_prose() {
        let seen = HashMap::new();
        let p = |t: &str| Paragraph { heading: None, text: t.to_string() };
        assert_eq!(prefilter(&p("Too few words here."), &seen), Some(PrefilterReason::TooShort));
        let links = "See https://example.com/a/very/long/path/one https://example.com/another/long/path/two https://example.com/third/long/path/three https://example.com/fourth/long/path/four https://example.com/fifth/long/path/five and a few more words to pass the floor";
        assert_eq!(prefilter(&p(links), &seen), Some(PrefilterReason::MostlyLinks));
        let digits = "2023 4090 24GB 16384 2520 MHz 450W 1599 2022 3090 24GB 10496 1695 MHz 350W 1499 12 13";
        assert_eq!(prefilter(&p(digits), &seen), Some(PrefilterReason::MostlyDigits));
        let nav = "Home\nNews\nSport\nWeather\nAbout us\nContact the team today\nCareers\nPodcasts and video\nNewsletters\nShop";
        assert_eq!(prefilter(&p(nav), &seen), Some(PrefilterReason::ShortLines));
        let refs = "[1] Smith, J. Ports. doi:10.1/abc for the full text\n[2] Jones, K. Walls. doi:10.2/def for the full text\n[3] Lee. Retrieved 2020 from the archive in full\n[4] Archived from the original on 2019 in full";
        assert_eq!(prefilter(&p(refs), &seen), Some(PrefilterReason::References));
        let cookie = "We use cookies to improve your experience on this site. By continuing you agree to our privacy policy.";
        assert_eq!(prefilter(&p(cookie), &seen), Some(PrefilterReason::Boilerplate));
        let good = "Memory bandwidth, not raw compute, sets the price of accelerators for large models, because each generated token must stream every weight through the chip once.";
        assert_eq!(prefilter(&p(good), &seen), None);
    }

    #[test]
    fn repeated_paragraphs_are_dropped() {
        let footer = "This article was produced by the newsroom team and is updated whenever new information becomes available to us.";
        let ps = vec![
            Paragraph { heading: None, text: footer.into() },
            Paragraph { heading: None, text: footer.into() },
        ];
        let seen = occurrence_counts(&ps);
        assert_eq!(prefilter(&ps[0], &seen), Some(PrefilterReason::Repeated));
    }

    #[test]
    fn chunks_hold_two_paragraphs_and_600_words_whichever_is_longer() {
        // Two long paragraphs (400 words each): two paragraphs AND 600 words.
        let kept = vec![prose(400, "a"), prose(400, "b"), prose(400, "c")];
        let chunks = group_chunks(&kept);
        // The third paragraph can't make a chunk on its own, so it joins.
        assert_eq!(chunks.len(), 1);

        // Many short paragraphs: the 600-word floor decides.
        let kept: Vec<String> = (0..20).map(|i| prose(100, &format!("p{i}"))).collect();
        let chunks = group_chunks(&kept);
        assert!(chunks.len() >= 2);
        for c in &chunks {
            assert!(word_count(c) >= CHUNK_MIN_WORDS);
        }

        // Too little for one full chunk: still one chunk.
        assert_eq!(group_chunks(&[prose(50, "x")]).len(), 1);
        assert!(group_chunks(&[]).is_empty());
    }

    #[test]
    fn splitting_is_deterministic() {
        let text = format!("# A\n\n{}\n\n{}", prose(300, "q"), prose(90, "r"));
        assert_eq!(split_paragraphs(&text), split_paragraphs(&text));
    }
}
