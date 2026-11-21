//! The deliverable's text: finding citations, checking them against the
//! ledger, and turning ledger citations into a numbered source list. Pure, so
//! the rules that decide what reaches the user are tested without a model.

use std::collections::{BTreeMap, BTreeSet};

use once_cell::sync::Lazy;
use regex::Regex;

use crate::research::ledger::{Body, Ledger};

/// `[L12]`, `[L12, L40]`, `[L12; L40]`.
static CITE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[\s*L\d+(?:\s*[,;]\s*L?\d+)*\s*\]").expect("cite regex"));
static NUM: Lazy<Regex> = Lazy::new(|| Regex::new(r"\d+").expect("num regex"));

/// Sentences shorter than this need no citation: connective tissue, not
/// claims.
const CLAIM_WORDS: usize = 12;

/// Every entry a text cites, in order of first appearance.
pub fn cited(text: &str) -> Vec<i64> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for m in CITE.find_iter(text) {
        for n in NUM.find_iter(m.as_str()) {
            if let Ok(seq) = n.as_str().parse::<i64>() {
                if seen.insert(seq) {
                    out.push(seq);
                }
            }
        }
    }
    out
}

/// The prose sentences of a Markdown text: headings, table rows and blank
/// lines are skipped; list items count as sentences of their own.
pub fn sentences(markdown: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in markdown.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('|') {
            continue;
        }
        let line = line
            .trim_start_matches(['-', '*', '+'])
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start_matches(['.', ')'])
            .trim();
        let mut current = String::new();
        let chars: Vec<char> = line.chars().collect();
        for (i, c) in chars.iter().enumerate() {
            current.push(*c);
            let ends =
                matches!(c, '.' | '!' | '?') && chars.get(i + 1).is_none_or(|n| n.is_whitespace());
            if ends {
                let s = current.trim().to_string();
                if !s.is_empty() {
                    out.push(s);
                }
                current.clear();
            }
        }
        let s = current.trim().to_string();
        if !s.is_empty() {
            out.push(s);
        }
    }
    out
}

/// What the mechanical citation check found in one section.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CheckReport {
    /// Cited ids with no entry behind them.
    pub unknown: Vec<i64>,
    /// Cited entries that are not usable findings (a source, a lead, a
    /// duplicate, a superseded claim).
    pub unusable: Vec<i64>,
    /// Claim-length sentences that cite nothing.
    pub uncited: Vec<String>,
    /// Usable findings the text cites.
    pub cited: Vec<i64>,
}

impl CheckReport {
    pub fn is_clean(&self) -> bool {
        self.unknown.is_empty() && self.unusable.is_empty() && self.uncited.is_empty()
    }

    /// The problems, as instructions to a writer revising the section.
    pub fn as_instructions(&self) -> Vec<String> {
        let mut out = Vec::new();
        for s in &self.unknown {
            out.push(format!(
                "[L{s}] does not exist: remove it and anything resting on it."
            ));
        }
        for s in &self.unusable {
            out.push(format!(
                "[L{s}] is not a usable finding (a source, a lead, a duplicate or a superseded \
                 claim): cite the finding itself, or drop the claim."
            ));
        }
        for s in &self.uncited {
            out.push(format!(
                "Cites nothing: \"{s}\" — add the finding it rests on, or cut it."
            ));
        }
        out
    }
}

pub fn check(markdown: &str, ledger: &Ledger) -> CheckReport {
    let mut report = CheckReport::default();
    for seq in cited(markdown) {
        if ledger.get(seq).is_none() {
            report.unknown.push(seq);
        } else if !ledger.is_usable_finding(seq) {
            report.unusable.push(seq);
        } else {
            report.cited.push(seq);
        }
    }
    for s in sentences(markdown) {
        if !CITE.is_match(&s) && s.split_whitespace().count() >= CLAIM_WORDS {
            report.uncited.push(s);
        }
    }
    report
}

/// `markdown` without `sentence`, or marked "(unverified)" when the contract
/// lets unverified claims stay. Returns the text unchanged when the sentence
/// is not found verbatim.
pub fn settle_sentence(markdown: &str, sentence: &str, keep_marked: bool) -> String {
    let sentence = sentence.trim();
    if sentence.is_empty() || !markdown.contains(sentence) {
        return markdown.to_string();
    }
    if keep_marked {
        markdown.replacen(sentence, &format!("{sentence} *(unverified)*"), 1)
    } else {
        let out = markdown.replacen(sentence, "", 1);
        // Tidy the gap the sentence left.
        out.replace("  ", " ").replace(" \n", "\n")
    }
}

/// One numbered source in the deliverable.
#[derive(Debug, Clone, PartialEq)]
pub struct NumberedSource {
    pub number: usize,
    pub title: String,
    pub url: String,
}

/// Replace ledger citations with numbered source references (`[L12]` → `[3]`,
/// numbered by first appearance) and return the source list. A cited finding
/// with no source entry loses its marker rather than pointing nowhere.
pub fn number_sources(markdown: &str, ledger: &Ledger) -> (String, Vec<NumberedSource>) {
    let mut numbers: BTreeMap<i64, usize> = BTreeMap::new();
    let mut list: Vec<NumberedSource> = Vec::new();
    let mut source_number = |finding: i64| -> Option<usize> {
        let src = *ledger.sources_of(finding).first()?;
        if let Some(n) = numbers.get(&src) {
            return Some(*n);
        }
        let Some(Body::Source(s)) = ledger.get(src).map(|e| &e.body) else {
            return None;
        };
        let n = list.len() + 1;
        numbers.insert(src, n);
        list.push(NumberedSource {
            number: n,
            title: if s.title.is_empty() {
                s.url.clone()
            } else {
                s.title.clone()
            },
            url: s.url.clone(),
        });
        Some(n)
    };
    let text = CITE
        .replace_all(markdown, |caps: &regex::Captures<'_>| {
            let mut ns: Vec<usize> = NUM
                .find_iter(&caps[0])
                .filter_map(|m| m.as_str().parse::<i64>().ok())
                .filter_map(&mut source_number)
                .collect();
            ns.dedup();
            let mut seen = BTreeSet::new();
            ns.retain(|n| seen.insert(*n));
            if ns.is_empty() {
                String::new()
            } else {
                format!(
                    "[{}]",
                    ns.iter()
                        .map(|n| n.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        })
        .into_owned();
    // A dropped marker can leave "claim ." behind.
    let text = text.replace(" .", ".").replace(" ,", ",");
    (text, list)
}

/// A file name for the deliverable.
pub fn file_slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.trim_matches('-').chars().take(60).collect();
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "research".into()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::research::ledger::tests::{finding, lead, link, row, source};

    fn ledger() -> Ledger {
        Ledger::build(
            vec![
                row(
                    1,
                    1,
                    &finding("Robbie appeared in 1940", "https://isfdb.org/t/1"),
                ),
                row(2, 1, &source("https://isfdb.org/t/1")),
                row(3, 1, &finding("Same as L1", "https://wiki.org/r")),
                row(4, 1, &lead("Check radio", true)),
                row(5, 1, &finding("Asimov wrote it", "https://isfdb.org/t/1")),
            ],
            vec![
                link(1, 2, "cites"),
                link(3, 1, "duplicates"),
                link(5, 2, "cites"),
            ],
            vec![],
        )
    }

    #[test]
    fn citations_are_found_in_every_bracket_form() {
        assert_eq!(
            cited("a [L3] b [L1, L4] c [L4; 9] d [L3]"),
            vec![3, 1, 4, 9]
        );
        assert!(cited("no cites, [3] or [Lx]").is_empty());
    }

    #[test]
    fn a_fabricated_citation_is_caught() {
        let l = ledger();
        let text = "Robbie first appeared in Super Science Stories in September 1940 [L1]. \
                    It was later collected [L99]. A lead is not evidence [L4]. A duplicate \
                    is not either [L3].\n\nThis long sentence makes a claim about the history \
                    of robot fiction without any citation at all.";
        let r = check(text, &l);
        assert_eq!(r.unknown, vec![99]);
        assert_eq!(r.unusable, vec![4, 3]);
        assert_eq!(r.cited, vec![1]);
        assert_eq!(r.uncited.len(), 1, "{:?}", r.uncited);
        assert!(!r.is_clean());
        assert_eq!(r.as_instructions().len(), 4);
        assert!(check("Short line. ## Heading", &l).is_clean());
    }

    #[test]
    fn sentences_skip_headings_and_split_lists() {
        let s = sentences("# Title\n\nOne. Two! Three?\n- Item one\n| a | b |\n3. Numbered.");
        assert_eq!(s, vec!["One.", "Two!", "Three?", "Item one", "Numbered."]);
    }

    #[test]
    fn unverified_sentences_are_cut_or_marked() {
        let md = "Keep this. Drop this one. Keep that.";
        assert_eq!(
            settle_sentence(md, "Drop this one.", false),
            "Keep this. Keep that."
        );
        assert_eq!(
            settle_sentence(md, "Drop this one.", true),
            "Keep this. Drop this one. *(unverified)* Keep that."
        );
        assert_eq!(settle_sentence(md, "Not here.", false), md);
    }

    #[test]
    fn sources_are_numbered_by_first_use_and_shared() {
        let l = ledger();
        let (text, list) = number_sources("A [L1]. B [L5]. C [L1, L5]. D [L4].", &l);
        assert_eq!(text, "A [1]. B [1]. C [1]. D.");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].url, "https://isfdb.org/t/1");
    }

    #[test]
    fn slugs_are_file_safe() {
        assert_eq!(
            file_slug("AI in SF, 1940–1970: a bibliography!"),
            "ai-in-sf-1940-1970-a-bibliography"
        );
        assert_eq!(file_slug("???"), "research");
    }
}
