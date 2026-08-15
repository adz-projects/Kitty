//! Tool calling for the in-process engine, on one protocol we control.
//!
//! `llama-cpp-2` binds only the plain C `llama_chat_apply_template`, which
//! takes `(role, content)` pairs and has no tools parameter — the tools-aware
//! path (`common_chat_templates_apply`, with its per-family parsers for
//! Hermes / Llama 3.x / Qwen / …) is C++ and unbound. Rather than
//! re-implement that moving target per model family, we render the prompt
//! ourselves and impose one syntax:
//!
//! ```text
//! <tool_call>{"name": "<tool>", "arguments": { … }}</tool_call>
//! ```
//!
//! This is near-native for the Hermes/Qwen/ChatML GGUFs that dominate local
//! use, and merely instruction-following for the rest. What makes it reliable
//! at 1–4B is not the prompt but the grammar: [`tools_grammar`] builds a GBNF
//! from the *real* tool schemas, and the caller feeds it to
//! `LlamaSampler::grammar_lazy` triggered on `<tool_call>`. Generation is
//! unconstrained during prose; the moment the model commits to a call it
//! *cannot* invent a tool name or an unknown argument key. That is the
//! difference between "unreliable" and "wrong".
//!
//! Everything here is pure and GGUF-free on purpose — it is where nearly all
//! the risk lives, so it is the part that unit tests can actually reach.

use std::collections::HashSet;

use serde_json::{json, Value};
use uuid::Uuid;

use crate::provider::base::ToolCall;
use crate::provider::tag_split::longest_tag_prefix_suffix;

pub const OPEN: &str = "<tool_call>";
pub const CLOSE: &str = "</tool_call>";
/// A tool *result* fed back on the next turn. Rendered as a user-role block
/// (see `flatten_for_template` in `provider.rs`) because the plain C chat
/// template errors or silently drops an unknown `tool` role on most templates.
pub const RESPONSE_OPEN: &str = "<tool_response>";
pub const RESPONSE_CLOSE: &str = "</tool_response>";

/// The `name`/`description`/`parameters` of one OpenAI-shaped tool definition
/// (`{"type":"function","function":{…}}`), or of a bare `{name,…}` form.
fn tool_parts(tool: &Value) -> Option<(&str, &str, Value)> {
    let f = tool.get("function").unwrap_or(tool);
    let name = f.get("name").and_then(|n| n.as_str())?.trim();
    if name.is_empty() {
        return None;
    }
    let desc = f.get("description").and_then(|d| d.as_str()).unwrap_or("");
    let params = match f.get("parameters") {
        Some(p) if p.is_object() => p.clone(),
        _ => json!({ "type": "object", "properties": {} }),
    };
    Some((name, desc, params))
}

/// The tool-list preamble appended to a turn's leading system message.
///
/// Kept deliberately terse — every token here is context a small model pays
/// for on a turn it may not even call a tool. The one non-negotiable is the
/// exact emit syntax, since the scanner keys off it byte for byte.
pub fn tool_system_block(tools: &[Value]) -> String {
    let mut s = String::from(
        "\n\nYou can call tools. To call one, emit exactly this and nothing else on the line:\n",
    );
    s.push_str(OPEN);
    s.push_str(r#"{"name": "<tool>", "arguments": { ... }}"#);
    s.push_str(CLOSE);
    s.push_str("\nAvailable tools:\n");
    for tool in tools {
        if let Some((name, desc, params)) = tool_parts(tool) {
            s.push_str("- ");
            s.push_str(name);
            if !desc.is_empty() {
                s.push_str(": ");
                s.push_str(desc);
            }
            s.push_str("\n  parameters: ");
            s.push_str(&compact(&params));
            s.push('\n');
        }
    }
    s
}

/// A GBNF that matches `{"name": <one of the tool names>, "arguments": <that
/// tool's parameter schema>}`, built by handing a `oneOf` wrapper to
/// llama.cpp's own `json_schema_to_grammar`.
///
/// `None` when no tool yields a usable schema, or when the conversion fails —
/// the caller treats either as "generate unconstrained", so a schema
/// llama.cpp can't digest degrades to prompt-only tool calling rather than
/// failing the turn.
pub fn tools_grammar(tools: &[Value]) -> Option<String> {
    let branches: Vec<Value> = tools
        .iter()
        .filter_map(|t| {
            let (name, _desc, params) = tool_parts(t)?;
            Some(json!({
                "type": "object",
                "properties": {
                    // `enum` with a single value rather than `const`: both are
                    // valid JSON Schema, but `enum` is the older, more broadly
                    // supported keyword in json-schema-to-grammar.
                    "name": { "enum": [name] },
                    "arguments": params,
                },
                "required": ["name", "arguments"],
                "additionalProperties": false,
            }))
        })
        .collect();
    if branches.is_empty() {
        return None;
    }
    let schema = json!({ "oneOf": branches });
    match llama_cpp_2::json_schema_to_grammar(&schema.to_string()) {
        Ok(g) => Some(g),
        Err(e) => {
            tracing::warn!("tool schema did not convert to a grammar ({e}); generating unconstrained");
            None
        }
    }
}

/// One parsed tool call, or a reason it was rejected.
#[derive(Debug, Default)]
pub struct ScanOut {
    /// Text outside any `<tool_call>` span — the visible answer.
    pub text: String,
    /// Well-formed calls to a known tool.
    pub calls: Vec<ToolCall>,
    /// Human-readable reasons a span was not executed (unknown tool, bad
    /// JSON). Surfaced as visible text by the caller, never executed.
    pub errors: Vec<String>,
}

/// Incrementally pulls `<tool_call>…</tool_call>` spans out of a generated
/// stream, emitting the surrounding text as it goes.
///
/// Its own running buffer rather than a [`TagSplitter`](crate::provider::tag_split::TagSplitter):
/// that type concatenates every inside-span in a fragment into one string,
/// which is right for a reasoning trace but would merge two adjacent tool
/// calls into one unparseable blob. Here each span must stay separate.
pub struct ToolCallScanner {
    buf: String,
    allowed: HashSet<String>,
}

impl ToolCallScanner {
    pub fn new(tools: &[Value]) -> Self {
        let allowed = tools
            .iter()
            .filter_map(|t| tool_parts(t).map(|(n, _, _)| n.to_string()))
            .collect();
        Self {
            buf: String::new(),
            allowed,
        }
    }

    /// Feed one generated piece. Returns any completed calls plus the text
    /// that fell outside them; holds back an unclosed span (and a possible
    /// partial opening tag at the very end) for the next call.
    pub fn feed(&mut self, piece: &str) -> ScanOut {
        self.buf.push_str(piece);
        let mut out = ScanOut::default();
        loop {
            match self.buf.find(OPEN) {
                Some(open_at) => {
                    out.text.push_str(&self.buf[..open_at]);
                    let after_open = &self.buf[open_at + OPEN.len()..];
                    match after_open.find(CLOSE) {
                        Some(close_rel) => {
                            let payload = after_open[..close_rel].to_string();
                            match parse_call(&payload, &self.allowed) {
                                Ok(tc) => out.calls.push(tc),
                                Err(e) => out.errors.push(e),
                            }
                            let consumed = open_at + OPEN.len() + close_rel + CLOSE.len();
                            self.buf = self.buf[consumed..].to_string();
                            // Keep scanning — a fragment can carry several calls.
                        }
                        None => {
                            // Opened but not yet closed: retain from the open
                            // tag and wait for the rest of the span.
                            self.buf = self.buf[open_at..].to_string();
                            return out;
                        }
                    }
                }
                None => {
                    // No opening tag. Emit everything except a tail that could
                    // be the start of one split across the next piece.
                    let hold = longest_tag_prefix_suffix(&self.buf, OPEN);
                    let split_at = self.buf.len() - hold;
                    out.text.push_str(&self.buf[..split_at]);
                    self.buf = self.buf[split_at..].to_string();
                    return out;
                }
            }
        }
    }

    /// End of stream: whatever is still buffered is an unterminated span or a
    /// dangling partial tag. Return it as visible text — a truncated call
    /// (e.g. `max_tokens` hit mid-`<tool_call>`) must surface, never execute.
    pub fn flush(&mut self) -> String {
        std::mem::take(&mut self.buf)
    }
}

/// Parse one `<tool_call>` payload into a [`ToolCall`] the agent loop can
/// dispatch, or reject it with a reason.
fn parse_call(payload: &str, allowed: &HashSet<String>) -> Result<ToolCall, String> {
    let trimmed = payload.trim();
    let parsed: Value = serde_json::from_str(trimmed)
        .map_err(|e| format!("ignored a malformed tool call ({e}): {trimmed}"))?;
    let name = parsed
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| format!("ignored a tool call with no name: {trimmed}"))?
        .to_string();
    if !allowed.contains(&name) {
        return Err(format!("ignored a call to unknown tool '{name}'"));
    }
    // Missing arguments become `{}` — a no-arg tool is legitimate. A non-object
    // `arguments` is a malformed call, though: the loop passes it straight to
    // the tool, which expects a map.
    let arguments = match parsed.get("arguments") {
        None => json!({}),
        Some(v) if v.is_object() => v.clone(),
        Some(_) => return Err(format!("ignored a call whose arguments were not an object: {trimmed}")),
    };
    Ok(ToolCall {
        id: format!("call_{}", Uuid::new_v4()),
        r#type: "function".into(),
        function: json!({ "name": name, "arguments": arguments }),
    })
}

/// Compact single-line JSON for embedding a schema in the prompt.
fn compact(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "{}".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": "d",
                "parameters": { "type": "object", "properties": { "path": { "type": "string" } } }
            }
        })
    }

    fn names(tools: &[Value]) -> Vec<String> {
        // A scanner whose allowed set matches these tools.
        tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn extracts_a_whole_call_and_the_surrounding_text() {
        let tools = [tool("read_file")];
        let mut sc = ToolCallScanner::new(&tools);
        let out = sc.feed(r#"sure<tool_call>{"name":"read_file","arguments":{"path":"a"}}</tool_call> done"#);
        assert_eq!(out.text, "sure done");
        assert_eq!(out.calls.len(), 1);
        assert_eq!(out.calls[0].function["name"], "read_file");
        assert_eq!(out.calls[0].function["arguments"]["path"], "a");
        assert!(out.calls[0].id.starts_with("call_"));
        assert!(out.errors.is_empty());
        let _ = names(&tools);
    }

    #[test]
    fn keeps_two_adjacent_calls_separate() {
        // The exact case a shared TagSplitter would merge.
        let tools = [tool("a"), tool("b")];
        let mut sc = ToolCallScanner::new(&tools);
        let out = sc.feed(
            r#"<tool_call>{"name":"a","arguments":{}}</tool_call><tool_call>{"name":"b","arguments":{}}</tool_call>"#,
        );
        assert_eq!(out.calls.len(), 2);
        assert_eq!(out.calls[0].function["name"], "a");
        assert_eq!(out.calls[1].function["name"], "b");
    }

    #[test]
    fn reassembles_a_call_split_one_char_at_a_time() {
        // The local engine's real cadence: one token per piece.
        let tools = [tool("read_file")];
        let mut sc = ToolCallScanner::new(&tools);
        let mut text = String::new();
        let mut calls = Vec::new();
        for ch in r#"x<tool_call>{"name":"read_file","arguments":{}}</tool_call>y"#.chars() {
            let out = sc.feed(&ch.to_string());
            text.push_str(&out.text);
            calls.extend(out.calls);
        }
        text.push_str(&sc.flush());
        assert_eq!(text, "xy");
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn an_unterminated_span_becomes_text_on_flush_not_a_call() {
        let tools = [tool("read_file")];
        let mut sc = ToolCallScanner::new(&tools);
        let out = sc.feed(r#"<tool_call>{"name":"read_file""#);
        assert!(out.calls.is_empty());
        assert!(out.text.is_empty());
        // Truncated mid-call (e.g. max_tokens) — surfaced, never executed.
        assert_eq!(sc.flush(), r#"<tool_call>{"name":"read_file""#);
    }

    #[test]
    fn rejects_an_unknown_tool_and_bad_json_without_executing() {
        let tools = [tool("read_file")];
        let mut sc = ToolCallScanner::new(&tools);
        let out = sc.feed(r#"<tool_call>{"name":"rm_rf","arguments":{}}</tool_call>"#);
        assert!(out.calls.is_empty());
        assert_eq!(out.errors.len(), 1);
        assert!(out.errors[0].contains("unknown tool"));

        let mut sc = ToolCallScanner::new(&tools);
        let out = sc.feed(r#"<tool_call>{not json}</tool_call>"#);
        assert!(out.calls.is_empty());
        assert_eq!(out.errors.len(), 1);
        assert!(out.errors[0].contains("malformed"));
    }

    #[test]
    fn a_missing_arguments_key_defaults_to_empty_object() {
        let tools = [tool("noargs")];
        let mut sc = ToolCallScanner::new(&tools);
        let out = sc.feed(r#"<tool_call>{"name":"noargs"}</tool_call>"#);
        assert_eq!(out.calls.len(), 1);
        assert_eq!(out.calls[0].function["arguments"], json!({}));
    }

    #[test]
    fn text_that_merely_contains_an_angle_bracket_passes_through() {
        let tools = [tool("a")];
        let mut sc = ToolCallScanner::new(&tools);
        let out = sc.feed("1 < 2 and a > b");
        assert_eq!(out.text, "1 < 2 and a > b");
        assert!(out.calls.is_empty());
    }

    #[test]
    fn system_block_lists_each_tool_and_the_exact_syntax() {
        let tools = [tool("read_file"), tool("write_file")];
        let block = tool_system_block(&tools);
        assert!(block.contains("<tool_call>"));
        assert!(block.contains("</tool_call>"));
        assert!(block.contains("read_file"));
        assert!(block.contains("write_file"));
    }
}
