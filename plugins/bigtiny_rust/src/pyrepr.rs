//! Minimal Python `repr()` syntax parser — a Rust port of the frontend's
//! `src/lib/pyrepr.ts` (`parsePyRepr`/`tryParsePyRepr`).
//!
//! The Adaptive Pathway sidecar's `/decide` endpoint returns `str(some_dict)`
//! (Python repr), not JSON, so `serde_json` can't be used directly. A naive
//! quote-swap (`'` -> `"`) would break on any hint text containing an
//! apostrophe (e.g. "don't do this again") — precisely why this is a real
//! small recursive-descent parser instead of a regex replace.
//!
//! The daemon uses this to turn `decide`'s payload into structured JSON so it
//! can extract per-turn hint text for the tail-region injection in
//! `agent::loop_` and the context builders. Mirrors the TS implementation's
//! grammar and tolerance: `parse` returns `Ok(Value)` on well-formed input,
//! `Err` on anything unexpected — callers treat errors as "no hints" (zero
//! delta to the prompt), never as a failure.

use serde_json::{json, Value};

#[derive(Debug)]
pub struct PyReprError;

fn skip_ws(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && (s[i] as char).is_whitespace() {
        i += 1;
    }
    i
}

fn parse_string(s: &[u8], i: usize) -> Result<(Value, usize), PyReprError> {
    let quote = s[i];
    debug_assert!(quote == b'\'' || quote == b'"');
    let mut j = i + 1;
    let mut out = String::new();
    while j < s.len() && s[j] != quote {
        if s[j] == b'\\' && j + 1 < s.len() {
            let c = s[j + 1] as char;
            out.push(match c {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                '\\' => '\\',
                '\'' => '\'',
                '"' => '"',
                other => other,
            });
            j += 2;
        } else {
            out.push(s[j] as char);
            j += 1;
        }
    }
    if j >= s.len() || s[j] != quote {
        return Err(PyReprError);
    }
    Ok((json!(out), j + 1))
}

fn parse_number(s: &[u8], i: usize) -> Result<(Value, usize), PyReprError> {
    let start = i;
    let mut j = i;
    if j < s.len() && (s[j] == b'-' || s[j] == b'+') {
        j += 1;
    }
    while j < s.len() && (s[j] as char).is_ascii_digit() {
        j += 1;
    }
    // Optional fractional/exponent tail (handled loosely — floats are rare in
    // these payloads and any unparseable token is a hard Err, not a guess).
    if j < s.len() && s[j] == b'.' {
        j += 1;
        while j < s.len() && (s[j] as char).is_ascii_digit() {
            j += 1;
        }
    }
    if j < s.len() && (s[j] == b'e' || s[j] == b'E') {
        j += 1;
        if j < s.len() && (s[j] == b'-' || s[j] == b'+') {
            j += 1;
        }
        while j < s.len() && (s[j] as char).is_ascii_digit() {
            j += 1;
        }
    }
    let token = std::str::from_utf8(&s[start..j]).map_err(|_| PyReprError)?;
    let v: f64 = token.parse().map_err(|_| PyReprError)?;
    Ok((json!(v), j))
}

fn parse_value(s: &[u8], i: usize) -> Result<(Value, usize), PyReprError> {
    let i = skip_ws(s, i);
    if i >= s.len() {
        return Err(PyReprError);
    }
    let c = s[i];
    // Python `repr()` writes `True`/`False`/`None`; tolerate the lowercase
    // JSON spellings too, defensively, in case any caller passes `json.dumps`
    // output instead of `str(...)`.
    match c {
        b'{' => parse_dict(s, i),
        b'[' => parse_list(s, i),
        b'(' => parse_list(s, i),
        b'\'' | b'"' => parse_string(s, i),
        b'T' | b't'
            if s.len() >= i + 4 && s[i..i + 4].eq_ignore_ascii_case(b"true") =>
        {
            Ok((json!(true), i + 4))
        }
        b'F' | b'f'
            if s.len() >= i + 5 && s[i..i + 5].eq_ignore_ascii_case(b"false") =>
        {
            Ok((json!(false), i + 5))
        }
        b'N' | b'n'
            if s.len() >= i + 4 && s[i..i + 4].eq_ignore_ascii_case(b"none") =>
        {
            Ok((Value::Null, i + 4))
        }
        _ => parse_number(s, i),
    }
}

fn parse_collection_close(s: &[u8], close: u8) -> bool {
    s.first() == Some(&close)
}

fn parse_list(s: &[u8], i: usize) -> Result<(Value, usize), PyReprError> {
    let close = if s[i] == b'[' { b']' } else { b')' };
    let mut j = i + 1;
    let mut out: Vec<Value> = Vec::new();
    j = skip_ws(s, j);
    if parse_collection_close(&s[j..], close) {
        return Ok((json!(out), j + 1));
    }
    loop {
        let (v, k) = parse_value(s, j)?;
        out.push(v);
        j = skip_ws(s, k);
        match s.get(j) {
            Some(b',') => {
                j += 1;
                j = skip_ws(s, j);
                if parse_collection_close(&s[j..], close) {
                    return Ok((json!(out), j + 1));
                }
            }
            Some(c) if *c == close => return Ok((json!(out), j + 1)),
            _ => return Err(PyReprError),
        }
    }
}

fn parse_dict(s: &[u8], i: usize) -> Result<(Value, usize), PyReprError> {
    let mut j = i + 1;
    let mut out = serde_json::Map::new();
    j = skip_ws(s, j);
    if j >= s.len() {
        return Err(PyReprError);
    }
    if s[j] == b'}' {
        return Ok((json!(out), j + 1));
    }
    loop {
        let (k, kk) = parse_value(s, j)?;
        j = skip_ws(s, kk);
        if s.get(j) != Some(&b':') {
            return Err(PyReprError);
        }
        let (v, vv) = parse_value(s, j + 1)?;
        let key = match k {
            Value::String(s) => s,
            other => other.to_string(),
        };
        out.insert(key, v);
        j = skip_ws(s, vv);
        match s.get(j) {
            Some(b',') => {
                j += 1;
                j = skip_ws(s, j);
                if j >= s.len() {
                    return Err(PyReprError);
                }
                if s[j] == b'}' {
                    return Ok((json!(out), j + 1));
                }
            }
            Some(b'}') => return Ok((json!(out), j + 1)),
            _ => return Err(PyReprError),
        }
    }
}

/// Parse a full Python-repr string into a JSON value. `Err` on any
/// malformed/partial input — match the TS `tryParsePyRepr` contract.
pub fn parse(input: &str) -> Result<Value, PyReprError> {
    let bytes = input.as_bytes();
    let (value, end) = parse_value(bytes, 0)?;
    let rest = skip_ws(bytes, end);
    if rest != bytes.len() {
        return Err(PyReprError);
    }
    Ok(value)
}

/// Safe wrapper: malformed/unexpected output should never fail a caller.
pub fn try_parse(input: &str) -> Option<Value> {
    parse(input).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_format_result_shape_with_apostrophe() {
        let input = "{'hints': [{'text': \"don't do this\", 'confidence': 0.8, 'type': 'single', 'primitive': 'write', 'domain': 'dev', 'attribution_id': 'a1', 'edge_id': 'e1'}, {'text': 'use edit', 'confidence': 0.5, 'type': 'single'}], 'confidence': 0.6, 'novelty': 0.1, 'is_flow_state': false}";
        let v = parse(input).unwrap();
        let hints = v["hints"].as_array().unwrap();
        assert_eq!(hints.len(), 2);
        assert_eq!(hints[0]["text"], "don't do this");
        assert_eq!(hints[1]["text"], "use edit");
        assert_eq!(v["confidence"], 0.6);
        assert_eq!(v["is_flow_state"], false);
    }

    #[test]
    fn parses_booleans_and_none() {
        assert_eq!(parse("True").unwrap(), json!(true));
        assert_eq!(parse("False").unwrap(), json!(false));
        assert_eq!(parse("None").unwrap(), Value::Null);
    }

    #[test]
    fn parses_empty_containers_and_numbers() {
        assert_eq!(parse("[]").unwrap(), json!([]));
        assert_eq!(parse("{}").unwrap(), json!({}));
        assert_eq!(parse("-1.0").unwrap(), json!(-1.0));
        assert_eq!(parse("3").unwrap(), json!(3.0));
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(parse("").is_err());
        assert!(parse("[1, 2").is_err());
        assert!(parse("{'a': }").is_err());
        assert!(parse("'unterminated").is_err());
        assert!(try_parse("not python").is_none());
    }
}
