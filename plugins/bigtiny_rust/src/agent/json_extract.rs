//! Pull the first JSON object out of a free-text model response.
//!
//! Unconditional (not feature-gated): both the LiteRT-LM local summarizer *and*
//! the provider-router fallback in [`super::summarizer_chain`] need it, and only
//! the former is feature-gated. It lives here, shared, so the two never drift.

use serde_json::Value;

/// Needed even with a grammar-constrained decoder for the *unconstrained*
/// refill step, where a small model will happily wrap its JSON in prose or a
/// ```json fence — and unconditionally for any cloud/remote provider, which
/// has no constrained-decode option at all.
pub(crate) fn extract_json(raw: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(raw.trim()) {
        return Some(v);
    }
    let start = raw.find('{')?;
    // Scan for the matching close rather than taking the last `}` in the
    // string, which would swallow trailing prose containing a brace.
    let bytes = raw.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&raw[start..=i]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_bare_json() {
        let v = extract_json(r#"{"a":1}"#).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn extracts_json_from_prose_and_fences() {
        assert_eq!(
            extract_json("Sure, here you go:\n```json\n{\"a\": 1}\n```\nHope that helps!").unwrap(),
            json!({"a": 1})
        );
        assert_eq!(
            extract_json("The answer is {\"a\": 1} as requested.").unwrap(),
            json!({"a": 1})
        );
    }

    /// Braces inside a string value must not confuse the depth scanner into
    /// closing early or never closing.
    #[test]
    fn braces_inside_strings_do_not_confuse_the_scanner() {
        let raw = r#"{"note": "use {curly} braces like this", "n": 2}"#;
        let v = extract_json(raw).unwrap();
        assert_eq!(v["note"], "use {curly} braces like this");
        assert_eq!(v["n"], 2);
    }

    /// Trailing prose containing a brace (e.g. "...{done}") must not be
    /// mistaken for part of the JSON object.
    #[test]
    fn stops_at_the_matching_brace_not_the_last_one() {
        let raw = r#"{"a": 1} and that's the result {not json}"#;
        assert_eq!(extract_json(raw).unwrap(), json!({"a": 1}));
    }

    #[test]
    fn returns_none_when_there_is_no_json() {
        assert!(extract_json("no json here at all").is_none());
    }
}
