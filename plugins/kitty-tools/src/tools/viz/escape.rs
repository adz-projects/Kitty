//! Escaping applied at the emission boundary (inside `render::svg`/`render::table`
//! primitives), never at intake. Every user-supplied string reaches an SVG/HTML
//! document only through `escape_text`/`escape_attr`.

/// Escapes text placed between tags (`<text>foo</text>`, `<td>foo</td>`, `<caption>`).
/// Strips C0 control characters other than tab/newline, which are not valid in XML
/// text content and would otherwise produce a malformed SVG document.
pub fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\t' | '\n' => out.push(c),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

/// Escapes text placed inside a double-quoted attribute value (`aria-label="foo"`).
/// Superset of `escape_text` — also escapes quotes.
pub fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '\t' | '\n' => out.push(c),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

/// Single-pass `__NAME__` token substitution. Replaces the crate's former
/// `.replace("__A__", a).replace("__B__", b)` chains, which had a real bug: if a
/// caller-supplied value for `A` happened to contain the literal text `__B__`, the
/// second `.replace` call would substitute inside content that was supposed to be
/// opaque. Scanning the template once for `__NAME__` tokens and substituting from a
/// fixed lookup table closes that class of bug entirely — the values are never
/// re-scanned for further tokens.
pub fn render_template(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'_' && bytes.get(i + 1) == Some(&b'_') {
            if let Some((value, token_len)) = match_token(template, i, values) {
                out.push_str(value);
                i += token_len;
                continue;
            }
        }
        // Advance by one *char*, not one byte, to stay on UTF-8 boundaries.
        let ch_len = utf8_char_len(bytes[i]);
        out.push_str(&template[i..i + ch_len]);
        i += ch_len;
    }
    out
}

fn utf8_char_len(first_byte: u8) -> usize {
    if first_byte & 0x80 == 0 {
        1
    } else if first_byte & 0xE0 == 0xC0 {
        2
    } else if first_byte & 0xF0 == 0xE0 {
        3
    } else {
        4
    }
}

fn match_token<'a>(template: &str, start: usize, values: &[(&str, &'a str)]) -> Option<(&'a str, usize)> {
    for (name, value) in values {
        let token = format!("__{name}__");
        if template[start..].starts_with(&token) {
            return Some((value, token.len()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_text_handles_amp_lt_gt() {
        assert_eq!(escape_text("a & b < c > d"), "a &amp; b &lt; c &gt; d");
    }

    #[test]
    fn escape_text_strips_control_chars_but_keeps_tab_newline() {
        assert_eq!(escape_text("a\x00b\tc\nd\x1fe"), "abtcnde".replace("t", "\t").replace("n", "\n"));
    }

    #[test]
    fn escape_attr_handles_quotes() {
        assert_eq!(escape_attr(r#"say "hi" and 'bye'"#), "say &quot;hi&quot; and &#39;bye&#39;");
    }

    #[test]
    fn escape_text_neutralizes_script_tags() {
        let out = escape_text("<script>alert(1)</script>");
        assert!(!out.contains("<script"));
        assert!(out.contains("&lt;script"));
    }

    #[test]
    fn render_template_substitutes_all_tokens() {
        let out = render_template("<title>__TITLE__</title><body>__BODY__</body>", &[("TITLE", "T"), ("BODY", "B")]);
        assert_eq!(out, "<title>T</title><body>B</body>");
    }

    #[test]
    fn render_template_does_not_rescan_substituted_values() {
        // The historical bug: a TITLE value containing the literal "__BODY__"
        // must not cause BODY's content to be spliced into the title slot.
        let out = render_template("<title>__TITLE__</title><body>__BODY__</body>", &[("TITLE", "evil __BODY__ literal"), ("BODY", "real body")]);
        assert_eq!(out, "<title>evil __BODY__ literal</title><body>real body</body>");
    }

    #[test]
    fn render_template_preserves_utf8_around_tokens() {
        let out = render_template("日__X__本", &[("X", "本")]);
        assert_eq!(out, "日本本");
    }

    #[test]
    fn render_template_leaves_unknown_tokens_untouched() {
        let out = render_template("__KNOWN__ __UNKNOWN__", &[("KNOWN", "k")]);
        assert_eq!(out, "k __UNKNOWN__");
    }
}
