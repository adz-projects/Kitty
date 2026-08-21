//! Standardized JSON response envelope — a byte-shape-compatible Rust port
//! of `kitty_docs_web.py`'s `success_response`/`error_response`.
//!
//! **Deliberately duplicated** from `plugins/kitty-tools/src/envelope.rs`
//! rather than shared through a third crate, mirroring the Python precedent
//! (`kitty_docs_web.py`'s own header note: "duplicated from `lean_mcp.py`
//! rather than imported ... a shared package would cost more than it saves").
//! The Rust reasons are the same in kind: these ship as two separate frozen
//! binaries, and depending on `kitty-tools` for two small utility modules
//! would drag its `zip`/`quick-xml`/OOXML-asset dependency tree into this
//! binary for nothing.
//!
//! The two copies are **not** identical, and must not be "unified": the
//! auto-hint chain below is `kitty_docs_web.py`'s, which deliberately splits
//! the `SEARCH`/`SCRAPE` branch that `lean_mcp.py` (and therefore
//! `kitty-tools`) still combines. That split is the Track C fix called out
//! in `kitty_docs_web.py:114-120` — every scrape failure used to get search
//! advice ("Broaden search keywords…"). Since this crate replaces
//! `kitty-docs-web`'s tools, its hints are the behavior models see today.
//! `kitty-tools` also carries a dead `TARGET_NOT_FOUND` branch for parity
//! with `lean_mcp.py`; `kitty_docs_web.py` has no such branch, so neither
//! does this file.
//!
//! `data`/`metadata` are always present when given, including falsy values
//! (`false`, `null`, empty maps) — Python's `if metadata:`/`if message:`
//! checks are truthiness checks, not `is None` checks, so an empty dict or
//! empty string is treated the same as "omit the field".

use serde_json::{json, Map, Value};

fn is_falsy(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        Value::Number(_) => false,
    }
}

/// Mirrors `success_response(data, message=None, truncated=False,
/// metadata=None)`. `truncated` and `data` are always present (matching
/// Python's unconditional `payload["truncated"]`/`payload["data"]`), even
/// when `data` is `false`/`null`/empty.
pub fn success_response(
    data: Value,
    message: Option<&str>,
    truncated: bool,
    metadata: Option<Value>,
) -> String {
    let mut payload = Map::new();
    payload.insert("status".to_string(), json!("success"));
    payload.insert("truncated".to_string(), json!(truncated));
    payload.insert("data".to_string(), data);

    if let Some(m) = message {
        if !m.is_empty() {
            payload.insert("message".to_string(), json!(m));
        }
    }
    if let Some(meta) = metadata {
        if !is_falsy(&meta) {
            payload.insert("metadata".to_string(), meta);
        }
    }

    serde_json::to_string_pretty(&Value::Object(payload)).unwrap_or_else(|_| "{}".to_string())
}

/// Mirrors `error_response(code, message, detail=None, hint=None)`,
/// including `kitty_docs_web.py`'s auto-hint chain. Branch order is ported
/// literally — `"NOT_FOUND" in code` is a substring test, so e.g.
/// `SEARCH_ID_NOT_FOUND` matches the *first* arm, not the `SEARCH` one.
/// Every call site that cares passes an explicit hint, which always wins.
pub fn error_response(
    code: &str,
    message: &str,
    detail: Option<&str>,
    hint: Option<&str>,
) -> String {
    let mut payload = Map::new();
    payload.insert("status".to_string(), json!("error"));
    payload.insert("error_code".to_string(), json!(code));
    payload.insert("message".to_string(), json!(message));

    if let Some(d) = detail {
        if !d.is_empty() {
            payload.insert("detail".to_string(), json!(d));
        }
    }

    let resolved_hint: Option<String> = hint.map(|h| h.to_string()).or_else(|| auto_hint(code));
    if let Some(h) = resolved_hint {
        if !h.is_empty() {
            payload.insert("hint".to_string(), json!(h));
        }
    }

    serde_json::to_string_pretty(&Value::Object(payload)).unwrap_or_else(|_| "{}".to_string())
}

fn auto_hint(code: &str) -> Option<String> {
    if code.contains("NOT_FOUND") || code.contains("MISSING") {
        Some(
            "Verify path spelling or call lean_analyze_workspace to check available files."
                .to_string(),
        )
    } else if code.contains("CORRUPT") || code.contains("PARSE") {
        Some("File may be damaged or password-protected. Verify format.".to_string())
    } else if code.contains("BAD_RANGE") || code.contains("OUT_OF_BOUNDS") {
        Some("Inspect dimensions or line counts before specifying bounds.".to_string())
    } else if code.contains("SEARCH") {
        Some("Broaden search keywords or check network connectivity.".to_string())
    } else if code.contains("SCRAPE") {
        Some(
            "Try a different URL, or use lean_web_search to find an alternative source."
                .to_string(),
        )
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_omits_falsy_message_and_metadata() {
        let s = success_response(json!("data"), Some(""), false, Some(json!({})));
        let v: Value = serde_json::from_str(&s).unwrap();
        assert!(v.get("message").is_none());
        assert!(v.get("metadata").is_none());
        assert_eq!(v["truncated"], json!(false));
        assert_eq!(v["data"], json!("data"));
    }

    #[test]
    fn success_includes_falsy_data_and_truncated_always() {
        let s = success_response(json!(false), None, true, None);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["data"], json!(false));
        assert_eq!(v["truncated"], json!(true));
    }

    #[test]
    fn scrape_and_search_get_distinct_hints() {
        // The Track C split this crate inherits from kitty_docs_web.py, and
        // the one place its envelope intentionally diverges from
        // kitty-tools' copy — a scrape failure must not get search advice.
        let scrape = error_response("SCRAPE_EMPTY", "no body", None, None);
        let search = error_response("SEARCH_FAILED", "nope", None, None);
        let scrape: Value = serde_json::from_str(&scrape).unwrap();
        let search: Value = serde_json::from_str(&search).unwrap();
        assert!(scrape["hint"].as_str().unwrap().contains("different URL"));
        assert!(search["hint"]
            .as_str()
            .unwrap()
            .contains("Broaden search keywords"));
    }

    #[test]
    fn not_found_substring_wins_over_search_branch() {
        // `SEARCH_ID_NOT_FOUND` contains both "NOT_FOUND" and "SEARCH";
        // Python's branch order means the first one wins. Pinned because a
        // "tidied" reordering would silently change it.
        let s = error_response("SEARCH_ID_NOT_FOUND", "missing", None, None);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert!(v["hint"]
            .as_str()
            .unwrap()
            .contains("lean_analyze_workspace"));
    }

    #[test]
    fn explicit_hint_wins_over_auto_hint() {
        let s = error_response("SEARCH_ID_NOT_FOUND", "missing", None, Some("custom hint"));
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["hint"], json!("custom hint"));
    }
}
