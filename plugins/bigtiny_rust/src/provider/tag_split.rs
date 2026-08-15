//! Splitting a token stream on a fixed open/close tag pair, across fragments.
//!
//! Reasoning models mark their thinking with `<think>...</think>` inline in
//! the content stream, and the tag almost never arrives whole: an SSE provider
//! splits it across deltas, and the in-process llama.cpp engine emits one
//! *token* at a time, so `<think>` routinely shows up as `<` / `think` / `>`.
//! Recognizing that as one tag needs state carried between fragments, which is
//! the entire reason this is a struct and not a function.
//!
//! Extracted from `openai_compat`, which had the only copy. A second
//! hand-rolled copy for the local provider would have been a second place to
//! get the cross-fragment bookkeeping subtly wrong — and the local path is the
//! one that stresses it hardest.

/// The result of feeding one fragment.
pub struct Split {
    /// Text that fell outside the tag pair.
    pub outside: String,
    /// Text that fell inside it. May be a partial span — the closing tag can
    /// arrive many fragments later.
    pub inside: String,
}

/// Incremental splitter over a fixed tag pair.
pub struct TagSplitter {
    open: &'static str,
    close: &'static str,
    /// Whether the previous fragment left us mid-span. Resetting this
    /// per-fragment (an old bug here) meant every fragment after the one
    /// containing the opening tag was treated as ordinary output.
    inside: bool,
    /// Trailing text held back because it could be the start of a tag split
    /// across a fragment boundary; prepended to the next fragment.
    pending: String,
}

impl TagSplitter {
    pub const fn new(open: &'static str, close: &'static str) -> Self {
        Self {
            open,
            close,
            inside: false,
            pending: String::new(),
        }
    }

    /// The `<think>`/`</think>` pair used for reasoning traces.
    pub fn thinking() -> Self {
        Self::new("<think>", "</think>")
    }

    pub fn is_inside(&self) -> bool {
        self.inside
    }

    /// Feed one fragment. A single `str::find` scan per tag occurrence, not a
    /// re-scan per character.
    pub fn feed(&mut self, content: &str) -> Split {
        let combined = if self.pending.is_empty() {
            content.to_string()
        } else {
            let mut s = std::mem::take(&mut self.pending);
            s.push_str(content);
            s
        };

        let mut outside = String::new();
        let mut inside = String::new();
        let mut rest: &str = &combined;

        loop {
            let needle = if self.inside { self.close } else { self.open };
            match rest.find(needle) {
                Some(idx) => {
                    let (before, after) = rest.split_at(idx);
                    if self.inside {
                        inside.push_str(before);
                    } else {
                        outside.push_str(before);
                    }
                    self.inside = !self.inside;
                    rest = &after[needle.len()..];
                }
                None => {
                    // Hold back any tail that could be the head of `needle`.
                    let hold = longest_tag_prefix_suffix(rest, needle);
                    let (keep, hold_str) = rest.split_at(rest.len() - hold);
                    if self.inside {
                        inside.push_str(keep);
                    } else {
                        outside.push_str(keep);
                    }
                    self.pending = hold_str.to_string();
                    break;
                }
            }
        }

        Split { outside, inside }
    }

    /// Release any held-back partial tag at end of stream, as literal text.
    ///
    /// Without this a stream ending in a dangling `<thi` drops those bytes
    /// silently — they are held pending a tag that never completes.
    pub fn flush(&mut self) -> String {
        std::mem::take(&mut self.pending)
    }
}

/// Length of the longest suffix of `s` that is also a proper prefix of `tag`.
///
/// `pub(crate)` because the local tool-call scanner needs the same
/// "this tail might be the start of a marker split across the next chunk"
/// judgement, and one copy is one place to get the char-boundary handling
/// right.
pub(crate) fn longest_tag_prefix_suffix(s: &str, tag: &str) -> usize {
    let max = tag.len().saturating_sub(1).min(s.len());
    for len in (1..=max).rev() {
        if s.is_char_boundary(s.len() - len) && s.ends_with(&tag[..len]) {
            return len;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_a_whole_tag_pair_in_one_fragment() {
        let mut s = TagSplitter::thinking();
        let out = s.feed("before<think>reasoning</think>after");
        assert_eq!(out.outside, "beforeafter");
        assert_eq!(out.inside, "reasoning");
        assert!(!s.is_inside());
    }

    #[test]
    fn carries_a_span_across_fragments() {
        let mut s = TagSplitter::thinking();
        assert_eq!(s.feed("<think>one").inside, "one");
        assert!(s.is_inside());
        // The bug this pins: everything after the opening fragment used to be
        // treated as ordinary output.
        assert_eq!(s.feed(" two").inside, " two");
        let last = s.feed("</think>done");
        assert_eq!(last.inside, "");
        assert_eq!(last.outside, "done");
    }

    #[test]
    fn recognizes_a_tag_split_one_character_at_a_time() {
        // The local engine's worst case: one token per fragment.
        let mut s = TagSplitter::thinking();
        let mut outside = String::new();
        let mut inside = String::new();
        for ch in "a<think>b</think>c".chars() {
            let out = s.feed(&ch.to_string());
            outside.push_str(&out.outside);
            inside.push_str(&out.inside);
        }
        outside.push_str(&s.flush());
        assert_eq!(outside, "ac");
        assert_eq!(inside, "b");
    }

    #[test]
    fn flush_releases_a_dangling_partial_tag_as_text() {
        let mut s = TagSplitter::thinking();
        let out = s.feed("tail<thi");
        assert_eq!(out.outside, "tail", "the partial tag is held back");
        assert_eq!(s.flush(), "<thi", "and released rather than dropped");
        assert_eq!(s.flush(), "", "flush is not repeatable");
    }

    #[test]
    fn text_that_merely_resembles_a_tag_passes_through() {
        let mut s = TagSplitter::thinking();
        let out = s.feed("1 < 2 and 3 > 2");
        assert_eq!(out.outside, "1 < 2 and 3 > 2");
        assert_eq!(out.inside, "");
    }
}
