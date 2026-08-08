//! Anti-sycophancy: visible signals the user can correct, driven by minimal
//! rule-based signals that write *no* beliefs (per the plan -- inferring
//! preferences directly from rules is precisely the
//! habituation-mistaken-for-preference failure this design avoids).

use crate::recall::FOOTER;

/// Basic structural features of a user message, fed to plateau entropy and
/// polarity hints. Pure functions -- no state, no belief writes.
#[derive(Debug, Clone, Default)]
pub struct MessageSignals {
    pub reply_len_bucket: usize,
    pub has_question: bool,
    pub has_list: bool,
    pub user_len_delta: i64,
}

pub fn bucket_reply_length(chars: usize) -> usize {
    if chars < 50 {
        0
    } else if chars < 200 {
        1
    } else {
        2
    }
}

/// Detect a lexical correction trigger (e.g. "no, actually", "wrong", "not
/// quite", "actually I meant"). These only drive anti-sycophancy signals and
/// polarity hints, never beliefs.
pub fn lexical_correction(text: &str) -> bool {
    let t = text.to_lowercase();
    ["no, actually", "wrong", "not quite", "actually i meant", "that's not", "thats not"]
        .iter()
        .any(|k| t.contains(k))
}

/// The `[Check yourself]` curiosity nudge, with a 14-day dismissal cooldown.
/// Returns None when the cooldown hasn't elapsed or the platform isn't
/// detected.
///
/// Returns the *bare* sentence -- the `[Check yourself]` label is applied by
/// `render_block`, same as the other three sections. It used to be baked in
/// here, which made this the one section `render_block` pushed raw and made
/// it unusable in `recall::render_reflection_block`, where a bracketed label
/// inside a `<think>` turn would read as injected scaffolding.
pub fn check_yourself(had_plateau: bool, last_dismissed_days_ago: i64) -> Option<String> {
    if !had_plateau || last_dismissed_days_ago < 14 {
        return None;
    }
    Some(
        "It's been a while since I challenged an assumption of mine. Is there anything \
         I've been taking for granted about this person that isn't true?"
            .to_string(),
    )
}

/// Whether to render `[Where I'm unsure]`. Cadence: every 12 exchanges.
pub fn unsure_due(exchange_count: i64) -> bool {
    exchange_count > 0 && exchange_count % 12 == 0
}

/// Assemble the full injected block. `exported = Some(block)` on a *turn*
/// where we render; the plan drives cadence at the call site. Returns the
/// block text or an empty string when nothing should be injected.
pub fn render_block(
    knows: &str,
    worth_testing: Option<String>,
    unsure: Option<String>,
    check: Option<String>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !knows.is_empty() {
        parts.push(format!("[Working assumptions about you]\n{knows}"));
    }
    if let Some(w) = worth_testing {
        if !w.is_empty() {
            parts.push(format!("[Worth testing this turn]\n{w}"));
        }
    }
    if let Some(u) = unsure {
        if !u.is_empty() {
            parts.push(format!("[Where I'm unsure]\n{u}"));
        }
    }
    if let Some(c) = check {
        if !c.is_empty() {
            parts.push(format!("[Check yourself]\n{c}"));
        }
    }
    if parts.is_empty() {
        return String::new();
    }
    parts.push(FOOTER.to_string());
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correction_triggers() {
        assert!(lexical_correction("No, actually I prefer shorter comments."));
        assert!(lexical_correction("That's not what I meant."));
        assert!(!lexical_correction("Can you refactor this?"));
    }

    #[test]
    fn check_yourself_cooldown() {
        assert!(check_yourself(true, 1).is_none());
        assert!(check_yourself(true, 15).is_some());
        assert!(check_yourself(false, 15).is_none());
    }

    #[test]
    fn unsure_cadence() {
        assert!(!unsure_due(0));
        assert!(!unsure_due(11));
        assert!(unsure_due(12));
        assert!(unsure_due(24));
    }

    #[test]
    fn render_empty_when_no_sections() {
        assert_eq!(
            render_block("", None, None, None),
            ""
        );
    }

    #[test]
    fn render_includes_footer_once() {
        let out = render_block("line", None, Some("uncertain".to_string()), None);
        assert!(out.contains(FOOTER));
        // exactly one footer
        assert_eq!(out.matches(FOOTER).count(), 1);
    }
}
