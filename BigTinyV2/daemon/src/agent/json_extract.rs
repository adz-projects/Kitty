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

/// Recover the complete part of a JSON object that was cut off mid-reply (the
/// model hit its output cap). Everything up to the last complete member or
/// element is kept, the cut-off tail is dropped, and the open containers are
/// closed. `None` when no object starts in `raw`, or when what is left still
/// does not parse.
///
/// For memory-slot extraction a partial object is far better than none: the
/// alternative was a chunk that could never be folded, because the next pass
/// built the same chunk and was cut off in the same place.
pub(crate) fn extract_truncated_json(raw: &str) -> Option<Value> {
    let start = raw.find('{')?;
    let text = &raw[start..];
    let bytes = text.as_bytes();
    // Open containers, and for each the byte offset just past its last
    // complete member (where a cut can safely be made).
    let mut stack: Vec<u8> = Vec::new();
    let mut safe_cut: Vec<usize> = Vec::new();
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate() {
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
            b'{' | b'[' => {
                stack.push(if b == b'{' { b'}' } else { b']' });
                safe_cut.push(i + 1);
            }
            b'}' | b']' => {
                stack.pop();
                safe_cut.pop();
                if stack.is_empty() {
                    return serde_json::from_str(&text[..=i]).ok();
                }
                // The container just closed is a complete member of its parent.
                if let Some(c) = safe_cut.last_mut() {
                    *c = i + 1;
                }
            }
            b',' => {
                if let Some(c) = safe_cut.last_mut() {
                    *c = i;
                }
            }
            _ => {}
        }
    }
    // Cut back to the innermost safe point, then close every open container.
    // A cut inside a nested container can leave its parent's half-written
    // member (`"key": [`), so try progressively outer cuts until one parses.
    for depth in (0..stack.len()).rev() {
        let mut candidate = text[..safe_cut[depth]].trim_end().to_string();
        if candidate.ends_with(',') {
            candidate.pop();
        }
        for closer in stack[..=depth].iter().rev() {
            candidate.push(*closer as char);
        }
        if let Ok(v) = serde_json::from_str::<Value>(&candidate) {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_truncated_object_keeps_its_complete_members() {
        let raw = r#"{"current_task_state": "fix it", "decision_rationale": ["a", "b"], "active_artifacts": {"spec": "long verbatim te"#;
        let v = extract_truncated_json(raw).unwrap();
        assert_eq!(v["current_task_state"], "fix it");
        assert_eq!(v["decision_rationale"], json!(["a", "b"]));
        assert!(v.get("active_artifacts").map_or(true, |a| a.as_object().is_some()));
    }

    #[test]
    fn a_truncated_array_keeps_its_complete_elements() {
        let raw = r#"{"a": 1, "list": ["one", "two", "thr"#;
        let v = extract_truncated_json(raw).unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["list"], json!(["one", "two"]));
    }

    #[test]
    fn a_complete_object_is_returned_as_is() {
        assert_eq!(extract_truncated_json(r#"x {"a": [1, 2]} y"#).unwrap(), json!({"a": [1, 2]}));
    }

    #[test]
    fn a_reply_with_no_object_is_none() {
        assert!(extract_truncated_json("no json").is_none());
    }

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
