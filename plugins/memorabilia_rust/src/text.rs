//! Deterministic text helpers shared by the ingestion pipeline (Stage 3
//! normalization + two-tier hashing, plan §3.1), the forget ladder
//! (tombstone keys, match normalization, plan §12.2), and cluster citations
//! (plan §3.4).
//!
//! Everything here is pure and deterministic: identical input always yields
//! identical output, which is what makes the hash tiers and tombstones work.

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// Stage 3 normalization (plan §3.1), case-preserving:
///
/// 1. Unicode NFC (compose combining marks canonically),
/// 2. collapse every whitespace run to a single space (and trim the ends),
/// 3. strip trailing punctuation.
///
/// It is the text `content_hash` is computed over, so "identical text always
/// yields identical `content_hash`" (plan §3.1) holds across re-scrapes of
/// the same content with different line breaks or trailing marks.
pub fn normalize_text(text: &str) -> String {
    let nfc: String = text.nfc().collect();
    let mut out = String::with_capacity(nfc.len());
    let mut prev_space = true; // leading whitespace collapsed away
    for ch in nfc.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    // Trailing-punctuation strip: a run of sentence-ending marks (with any
    // spaces between them) is a formatting artifact, not content.
    const TRAILING: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\''];
    loop {
        let trimmed = out.trim_end();
        if let Some(last) = trimmed.chars().last() {
            if TRAILING.contains(&last) {
                out.truncate(trimmed.len() - last.len_utf8());
                continue;
            }
        }
        break;
    }
    out.trim_end().to_string()
}

/// SHA-256 of `s` as a lowercase hex string (the plan §3.1 hash tier format
/// and the §12.2 tombstone key format).
pub fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Stable slug of a citation (plan §3.4): lowercase, keep `[a-z0-9.]`,
/// collapse every run of other characters to a single `_`, trim edge
/// underscores. `slug("Scraped: docs.python.org") == "scraped_docs.python.org"`
/// (the plan's own example). The same citation therefore always maps to the
/// same `provenance_cluster_id` across re-ingestions.
pub fn slug(citation: &str) -> String {
    let mut out = String::with_capacity(citation.len());
    for ch in citation.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' {
            out.push(ch);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

/// The `YYYY-MM-DD` capture date of an ISO-8601 UTC timestamp (plan §3.4:
/// the citation's date component). A timestamp that does not start with a
/// 10-char date component yields an empty string (the citation still
/// renders deterministically).
pub fn citation_date(captured_at: &str) -> &str {
    let Some(d) = captured_at.get(..10) else {
        return "";
    };
    let b = d.as_bytes();
    let digits = [0usize, 1, 2, 3, 5, 6, 8, 9].iter().all(|&i| b[i].is_ascii_digit());
    let dashes = b[4] == b'-' && b[7] == b'-';
    if digits && dashes {
        d
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_is_case_preserving_and_whitespace_collapsing() {
        assert_eq!(normalize_text("The  quick \t brown\nfox."), "The quick brown fox");
        // case preserved
        assert_eq!(normalize_text("KeepCase Here"), "KeepCase Here");
        // NFC: e + combining acute → precomposed é
        assert_eq!(normalize_text("cafe\u{0301}"), "café");
        // trailing punctuation (and the space before it) stripped
        assert_eq!(normalize_text("end of record .") , "end of record");
    }

    #[test]
    fn identical_text_same_hash() {
        assert_eq!(sha256_hex("abc"), sha256_hex("abc"));
        assert_ne!(sha256_hex("abc"), sha256_hex("abd"));
        // the normalized hash is what the pipeline uses
        assert_eq!(
            sha256_hex(&normalize_text("A  B\n")),
            sha256_hex(&normalize_text("A B"))
        );
    }

    #[test]
    fn slug_matches_plan_example() {
        assert_eq!(slug("Scraped: docs.python.org"), "scraped_docs.python.org");
        assert_eq!(slug("Slack: #infrastructure"), "slack_infrastructure");
        assert_eq!(slug("a  b // c"), "a_b_c");
    }

    #[test]
    fn citation_date_extracts_iso_date_part() {
        assert_eq!(citation_date("2026-08-01T10:00:00Z"), "2026-08-01");
        assert_eq!(citation_date("not-a-timestamp"), "");
    }
}
