//! Standardized JSON response envelope — a byte-shape-compatible Rust port
//! of `lean_mcp.py`'s `success_response`/`error_response`.
//!
//! `data`/`metadata` are always present when given, including falsy values
//! (`false`, `null`, empty maps) — Python's `if metadata:`/`if message:`
//! checks are truthiness checks, not `is None` checks, so an empty dict or
//! empty string is treated the same as "omit the field". `skip_if_falsy`
//! below reproduces that, since `serde`'s `skip_serializing_if =
//! "Option::is_none"` would emit e.g. `"message": ""` where Python omits it
//! entirely.

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
pub fn success_response(data: Value, message: Option<&str>, truncated: bool, metadata: Option<Value>) -> String {
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
/// including the auto-hint chain — **port the branch order literally**: the
/// dead `TARGET_NOT_FOUND` arm below is intentionally unreachable (`"NOT_FOUND"
/// in "TARGET_NOT_FOUND"` matches the first arm before it), matching
/// `lean_mcp.py:84-93` exactly. Do not "fix" the ordering — a golden test
/// pins this.
pub fn error_response(code: &str, message: &str, detail: Option<&str>, hint: Option<&str>) -> String {
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
        Some("Verify path spelling or call lean_analyze_workspace to check available files.".to_string())
    } else if code.contains("CORRUPT") || code.contains("PARSE") {
        Some("File may be damaged or password-protected. Verify format.".to_string())
    } else if code.contains("BAD_RANGE") || code.contains("OUT_OF_BOUNDS") {
        Some("Inspect dimensions or line counts before specifying bounds.".to_string())
    } else if code.contains("TARGET_NOT_FOUND") {
        // Dead branch, kept for parity — see doc comment above.
        Some("Use lean_file_read first to confirm exact string formatting or line numbers.".to_string())
    } else if code.contains("SEARCH") || code.contains("SCRAPE") {
        Some("Broaden search keywords or check domain connectivity.".to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn not_found_gets_the_workspace_hint() {
        let s = error_response("DOCX_NOT_FOUND", "missing", None, None);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert!(v["hint"].as_str().unwrap().contains("lean_analyze_workspace"));
    }

    #[test]
    fn target_not_found_dead_branch_still_matches_not_found_first() {
        // Pins the intentional-bug parity with lean_mcp.py: "NOT_FOUND" is a
        // substring of "TARGET_NOT_FOUND", so the first `contains` check
        // wins and the TARGET_NOT_FOUND-specific hint is unreachable.
        let s = error_response("TARGET_NOT_FOUND", "no match", None, None);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert!(v["hint"].as_str().unwrap().contains("lean_analyze_workspace"));
        assert!(!v["hint"].as_str().unwrap().contains("lean_file_read"));
    }

    #[test]
    fn explicit_hint_wins_over_auto_hint() {
        let s = error_response("DOCX_NOT_FOUND", "missing", None, Some("custom hint"));
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["hint"], json!("custom hint"));
    }
}
