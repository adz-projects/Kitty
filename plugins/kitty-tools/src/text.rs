//! Text helpers matching Python string-method semantics that Rust's stdlib
//! doesn't reproduce by default.

/// Python's `str.splitlines()` splits on `\n \r\n \r \v \f \x1c \x1d \x1e
/// \x85    ` — Rust's `str::lines()` only splits on `\n` (and
/// strips a trailing `\r`). Used anywhere `lean_mcp.py`'s Word tools call
/// `.splitlines()` on user-supplied markdown-lite text, so a stray `\r`,
/// vertical tab, or form feed doesn't silently get treated as part of the
/// previous line's content.
pub fn py_splitlines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                lines.push(std::mem::take(&mut current));
            }
            '\n' | '\u{0B}' | '\u{0C}' | '\u{1C}' | '\u{1D}' | '\u{1E}' | '\u{85}' | '\u{2028}'
            | '\u{2029}' => {
                lines.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_plain_newline() {
        assert_eq!(py_splitlines("a\nb\nc"), vec!["a", "b", "c"]);
    }

    #[test]
    fn splits_on_crlf_without_emitting_empty_line() {
        assert_eq!(py_splitlines("a\r\nb"), vec!["a", "b"]);
    }

    #[test]
    fn splits_on_lone_cr() {
        assert_eq!(py_splitlines("a\rb"), vec!["a", "b"]);
    }

    #[test]
    fn splits_on_vertical_tab_and_form_feed() {
        assert_eq!(py_splitlines("a\u{0B}b\u{0C}c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn no_trailing_empty_line_without_trailing_newline() {
        assert_eq!(py_splitlines("a\nb"), vec!["a", "b"]);
    }

    #[test]
    fn empty_input_yields_no_lines() {
        assert_eq!(py_splitlines(""), Vec::<String>::new());
    }
}
