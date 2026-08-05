//! Approximate text measurement for laying out SVG nodes without clipping.
//!
//! No font is embedded. A font crate (`fontdue`/`ab_glyph`) would need a bundled
//! font file (300KB-1MB) in an exe that is frozen and shipped as an `externalBin`,
//! and the precision it buys is illusory anyway: the iframe renders with whatever
//! `system-ui` resolves to on the *user's* machine (Segoe UI Variable on Windows,
//! SF Pro on macOS), not the font we'd embed. A static advance-width table (from
//! Helvetica's public AFM metrics, which track Segoe UI/SF Pro/Roboto for Latin
//! glyphs at UI sizes within a few percent) plus a safety margin gets the thing
//! that actually matters — no node clipping its own label — without that cost.

use std::collections::VecDeque;

/// ASCII 0x20..=0x7E advance widths, in 1/1000 em (Helvetica AFM metrics).
const ADVANCE_1000: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, // space ! " # $ % & ' ( ) * + , - . /
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556, // 0-9 : ; < = > ?
    1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778, // @ A-O
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556, // P-Z [ \ ] ^ _
    333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, // ` a-o
    556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584, // p-z { | } ~
];

/// Text in the node's own font-weight (600/700, semibold/bold) runs noticeably
/// wider than the regular-weight metrics above, on top of general hinting slack —
/// this margin biases every estimate upward, since under-estimating causes real
/// clipping and over-estimating only wraps a little earlier than strictly needed.
const SAFETY_MARGIN: f32 = 1.18;
/// Latin-1 Supplement through Cyrillic/Greek/etc. (U+00A0..U+2E7F): most run
/// close to the Latin average width at UI sizes.
const FALLBACK_LATIN: u16 = 550;
/// CJK and other fullwidth glyphs (U+2E80 and above) are roughly square.
const FALLBACK_WIDE: u16 = 1000;

fn char_advance_1000(c: char) -> u16 {
    let cp = c as u32;
    if (0x20..=0x7E).contains(&cp) {
        ADVANCE_1000[(cp - 0x20) as usize]
    } else if cp < 0x2E80 {
        FALLBACK_LATIN
    } else {
        FALLBACK_WIDE
    }
}

/// Estimated rendered width of a single char at `font_size` px, in px.
fn char_width_px(c: char, font_size: f32) -> f32 {
    char_advance_1000(c) as f32 / 1000.0 * font_size * SAFETY_MARGIN
}

/// Estimated rendered width of `s` at `font_size` px, in px.
pub fn measure_px(s: &str, font_size: f32) -> f32 {
    let em_1000: f32 = s.chars().map(|c| char_advance_1000(c) as f32).sum();
    em_1000 / 1000.0 * font_size * SAFETY_MARGIN
}

/// Greedy word-wrap of `s` into at most `max_lines` lines, each estimated to fit
/// within `max_px`. A single word wider than `max_px` is hard-broken at a
/// character boundary. If content remains after `max_lines` lines are filled, the
/// last line is truncated (character-wise) and suffixed with `…` so it still fits.
pub fn wrap(s: &str, max_px: f32, font_size: f32, max_lines: usize) -> Vec<String> {
    if max_lines == 0 {
        return Vec::new();
    }
    let max_px = max_px.max(1.0);
    let mut pending: VecDeque<String> = s.split_whitespace().map(|w| w.to_string()).collect();
    if pending.is_empty() {
        return Vec::new();
    }

    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0.0_f32;
    let space_width = measure_px(" ", font_size);

    while let Some(word) = pending.pop_front() {
        let word_width = measure_px(&word, font_size);

        if word_width > max_px {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                if lines.len() == max_lines {
                    pending.push_front(word);
                    break;
                }
            }
            current_width = split_wide_word(&word, max_px, font_size, &mut lines, &mut pending, max_lines, &mut current);
            continue;
        }

        if current.is_empty() {
            current = word;
            current_width = word_width;
        } else {
            let candidate_width = current_width + space_width + word_width;
            if candidate_width <= max_px {
                current.push(' ');
                current.push_str(&word);
                current_width = candidate_width;
            } else {
                lines.push(std::mem::take(&mut current));
                if lines.len() == max_lines {
                    pending.push_front(word);
                    break;
                }
                current = word;
                current_width = word_width;
            }
        }
    }

    if !current.is_empty() && lines.len() < max_lines {
        lines.push(current);
    }

    if !pending.is_empty() {
        if let Some(last) = lines.last_mut() {
            *last = ellipsize(last, max_px, font_size);
        }
    }

    lines
}

/// Splits an over-wide unbroken word into fixed-width chunks in a single
/// O(len) pass. The former head-recursion on the remaining token
/// (`hard_break_word` re-measuring the rest from scratch per chunk) made
/// wrapping a long unbroken token quadratic — an unbreakable ~1MB token
/// degenerated into re-scanning ~1MB for every ~15-char chunk. Measuring each
/// char's width once and cutting at the running total is linear. Full chunks
/// go onto `lines`; the final (fitting) chunk becomes `current`. Returns the
/// width of the final chunk, or `0.0` if a `max_lines` cutoff requeued the
/// remainder and left `current` empty.
fn split_wide_word(
    word: &str,
    max_px: f32,
    font_size: f32,
    lines: &mut Vec<String>,
    pending: &mut VecDeque<String>,
    max_lines: usize,
    current: &mut String,
) -> f32 {
    let mut start_byte = 0usize;
    let mut width = 0.0_f32;
    for (byte_idx, c) in word.char_indices() {
        let cw = char_width_px(c, font_size);
        if width + cw > max_px && byte_idx > start_byte {
            let chunk = &word[start_byte..byte_idx];
            if lines.len() == max_lines {
                pending.push_front(word[byte_idx..].to_string());
                return 0.0;
            }
            lines.push(chunk.to_string());
            start_byte = byte_idx;
            width = 0.0;
        }
        width += cw;
    }
    *current = word[start_byte..].to_string();
    width
}

/// Truncates `line` character-wise from the end until `line + "…"` fits `max_px`.
fn ellipsize(line: &str, max_px: f32, font_size: f32) -> String {
    let ellipsis_w = measure_px("…", font_size);
    if measure_px(line, font_size) + ellipsis_w <= max_px {
        return format!("{line}…");
    }
    let chars: Vec<char> = line.chars().collect();
    for cut in (0..chars.len()).rev() {
        let candidate: String = chars[..cut].iter().collect();
        if measure_px(&candidate, font_size) + ellipsis_w <= max_px {
            return format!("{candidate}…");
        }
    }
    "…".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measure_px_grows_with_length() {
        let a = measure_px("a", 14.0);
        let aa = measure_px("aa", 14.0);
        let aaa = measure_px("aaa", 14.0);
        assert!(a < aa);
        assert!(aa < aaa);
    }

    #[test]
    fn measure_px_grows_with_font_size() {
        assert!(measure_px("Hello", 10.0) < measure_px("Hello", 20.0));
    }

    #[test]
    fn wrap_never_exceeds_max_px() {
        let text = "The quick brown fox jumps over the lazy dog and keeps running";
        let max_px = 200.0;
        let font_size = 12.5;
        for line in wrap(text, max_px, font_size, 5) {
            assert!(measure_px(&line, font_size) <= max_px, "line {line:?} exceeds {max_px}px");
        }
    }

    #[test]
    fn wrap_hard_breaks_a_single_long_word() {
        let word = "x".repeat(60);
        let max_px = 100.0;
        let font_size = 12.5;
        let lines = wrap(&word, max_px, font_size, 10);
        assert!(lines.len() > 1, "expected the 60-char word to break across multiple lines");
        for line in &lines {
            assert!(measure_px(line, font_size) <= max_px);
        }
        // No characters lost across the break.
        assert_eq!(lines.concat().len(), word.len());
    }

    #[test]
    fn wrap_truncates_with_ellipsis_when_content_overflows_max_lines() {
        let text = "one two three four five six seven eight nine ten";
        let max_px = 60.0;
        let font_size = 12.5;
        let lines = wrap(text, max_px, font_size, 1);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].ends_with('…'));
        assert!(measure_px(&lines[0], font_size) <= max_px);
    }

    #[test]
    fn wrap_of_short_text_needs_no_truncation() {
        let lines = wrap("Ship it", 200.0, 12.5, 3);
        assert_eq!(lines, vec!["Ship it".to_string()]);
    }

    #[test]
    fn wrap_of_empty_or_whitespace_text_yields_no_lines() {
        assert!(wrap("", 200.0, 12.5, 3).is_empty());
        assert!(wrap("   ", 200.0, 12.5, 3).is_empty());
    }

    #[test]
    fn wrap_of_long_unbroken_token_is_linear_time() {
        // Regression: the old hard-break-and-requeue loop re-measured the
        // remaining token from scratch per ~15-char chunk, making a long
        // unbroken token quadratic (an ~1MB token took effectively forever).
        // Splitting by cumulative width once must return in well under the
        // budget with no characters lost.
        let word = "a".repeat(1_000_000);
        let start = std::time::Instant::now();
        let lines = wrap(&word, 200.0, 12.5, 1_000_000);
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 5000,
            "wrap of a 1MB token took {elapsed:?} -- likely quadratic behavior"
        );
        let echoed: usize = lines.iter().map(String::len).sum();
        assert!(echoed >= word.len(), "output length {echoed} < input length {}", word.len());
    }
}
