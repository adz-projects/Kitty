//! The built-in specialists, and the rules a definition must satisfy.
//!
//! # What earns a place here
//!
//! A specialist is worth having when the work is high-tool-call-volume relative
//! to the size of its answer, can succeed from a paragraph plus some handles
//! without the parent's conversation history, and produces something checkable.
//! It also has to be doable with tools that need no human approval — under
//! `hitl_policy: auto_reject` a specialist whose job requires an approval does
//! not fail loudly, it just returns a thinner answer.
//!
//! That last test is why there is no drafter, editor or file-writer here.
//! Writing tools are excluded deliberately, not by oversight -- and `lean_shell`
//! goes with them: `agent::loop_::is_write_tool` classifies it write-capable,
//! correctly, because nothing can establish that a given shell command is
//! read-only. A "read-only shell" for the locator would have been denied by
//! containment on its first call, producing a specialist that looked like it
//! worked. `no_builtin_can_write` below is what keeps that from being quietly
//! undone.

use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::models::specialist::Specialist;
use crate::storage::specialists as store;

/// Compose one specialist's answer schema.
///
/// Every specialist's schema carries `notes` and `refusals` alongside its own
/// fields. `refusals` is not decoration: a delegate that was blocked from a
/// tool call (see `agent::loop_`'s auto-reject path) would otherwise return a
/// quietly incomplete answer that reads exactly like a complete one.
///
/// `additionalProperties: false` and an all-inclusive `required` are what
/// OpenAI's `strict: true` demands (`provider::schema::directive_for`), and a
/// schema that omits either is rejected on the wire rather than downgraded. So
/// "nothing to report" is an empty array or string, never an absent key.
fn answer_schema(mut props: serde_json::Map<String, Value>) -> Value {
    props.insert(
        "notes".into(),
        json!({
            "type": "string",
            "description": "Anything the caller should know that the fields above do not capture. Empty string if none."
        }),
    );
    props.insert(
        "refusals".into(),
        json!({
            "type": "array",
            "description": "Tool calls you attempted that were refused or unavailable, and what you could not determine as a result. Empty array if none.",
            "items": {"type": "string"}
        }),
    );
    let required: Vec<String> = props.keys().cloned().collect();
    json!({
        "type": "object",
        "properties": Value::Object(props),
        "required": required,
        "additionalProperties": false,
    })
}

fn props(pairs: Vec<(&str, Value)>) -> serde_json::Map<String, Value> {
    pairs
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

fn builtin(
    name: &str,
    description: &str,
    system_prompt: &str,
    tool_allow: &[&str],
    schema: Value,
) -> Specialist {
    builtin_with_fan_out(name, description, system_prompt, tool_allow, schema, None)
}

/// As `builtin`, for the two whose work really is per-document.
///
/// `researcher`, `locator` and `analyst` deliberately do not split: locating
/// something *across* a corpus is one job that reads many files, not many jobs,
/// and splitting it would give each delegate a view too narrow to answer with.
fn builtin_with_fan_out(
    name: &str,
    description: &str,
    system_prompt: &str,
    tool_allow: &[&str],
    schema: Value,
    fan_out: Option<&str>,
) -> Specialist {
    Specialist {
        // Derived from the name, not random: re-seeding must not create a
        // second row, and a stable id is what lets a client link to a built-in
        // across daemon restarts and machines.
        id: format!("builtin:{name}"),
        app_id: None,
        name: name.to_string(),
        description: description.to_string(),
        system_prompt: system_prompt.to_string(),
        provider: None,
        model: None,
        tool_allow: tool_allow.iter().map(|s| s.to_string()).collect(),
        response_schema: Some(schema),
        max_steps: 20,
        fan_out: fan_out.map(str::to_string),
        max_concurrent: None,
        // Left to the daemon default rather than pinned per specialist: the
        // right budget depends on the model a delegate lands on, which a
        // definition cannot know.
        reasoning_cap: None,
        enabled: true,
        builtin: true,
    }
}

/// Shared preamble. Every specialist is told the same three things, because
/// every specialist fails the same three ways: padding a short answer,
/// pretending a blocked tool call succeeded, and re-reading a document the
/// caller already had extracted.
const COMMON: &str = "You are a specialist working on behalf of another agent, not a person. \
Answer only what was asked; the caller cannot see your working, so put everything that matters \
in your final structured answer and nothing that does not. If a tool call is refused or a source \
is unavailable, say so in `refusals` and continue with what you can — never present a partial \
result as a complete one. When the request gives you document ids or paths, work from those \
directly rather than asking for the content.";

pub fn researcher() -> Specialist {
    builtin(
        "researcher",
        "Answers a factual question that needs sources from the web or from installed retrieval \
         tools. Use for anything requiring current information, citations, or a survey of what \
         is out there. Returns findings with evidence and source URLs. Not for reasoning over \
         documents the user already gave you.",
        "You research a question and report what you found. Search, read, and corroborate before \
         answering. Every claim you report must be traceable to a source you actually opened — \
         if you could not verify something, put it in `open_questions` rather than asserting it. \
         Prefer a few well-sourced findings over many thin ones.",
        &[
            "lean_web_search",
            "lean_web_search_read_chunk",
            "lean_web_scrape",
        ],
        answer_schema(props(vec![
            (
                "findings",
                json!({
                    "type": "array",
                    "description": "What you established, most important first.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "claim": {"type": "string"},
                            "evidence": {"type": "string", "description": "What the source actually said."},
                            "source_url": {"type": "string"}
                        },
                        "required": ["claim", "evidence", "source_url"],
                        "additionalProperties": false
                    }
                }),
            ),
            (
                "open_questions",
                json!({
                    "type": "array",
                    "description": "What you could not establish, and why. Empty array if none.",
                    "items": {"type": "string"}
                }),
            ),
        ])),
    )
}

pub fn summarizer() -> Specialist {
    builtin_with_fan_out(
        "summarizer",
        "Reads a long document or a set of documents and returns notes on a specified section or \
         question. Use when the material is too long to put in context and you need its substance \
         rather than its exact words. Pass document ids or file paths. Returns a summary, key \
         points, and anchors back into the source.",
        "You read source material and report its substance. Work through the document with the \
         search and chunk tools rather than reading it end to end; the caller wants what the \
         document says about their request, not a précis of everything in it. Anchor every key \
         point to where you found it so the caller can go back to it. Do not speculate beyond \
         what the text supports.",
        &[
            "lean_doc_search",
            "lean_doc_read_chunk",
            "lean_file_read",
            "lean_pdf_read_text",
            "lean_pdf_read_outline",
            "lean_word_read_text",
            "lean_word_read_outline",
        ],
        answer_schema(props(vec![
            ("summary", json!({"type": "string"})),
            (
                "key_points",
                json!({"type": "array", "items": {"type": "string"}}),
            ),
            (
                "anchors",
                json!({
                    "type": "array",
                    "description": "Where in the source each key point came from.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "document_id": {"type": "string"},
                            "locator": {"type": "string", "description": "Page, section, or line range."}
                        },
                        "required": ["document_id", "locator"],
                        "additionalProperties": false
                    }
                }),
            ),
        ])),
        Some("per_ref"),
    )
}

pub fn locator() -> Specialist {
    builtin(
        "locator",
        "Finds where something lives across a workspace or a set of documents. Use when you need \
         to know which files or sections are relevant before reading anything. Returns paths and \
         anchors only, never file contents — read them yourself once you know where to look.",
        "You find where things are; you do not report what they say. Search with \
         lean_shell_ro (grep, find, ls) and the workspace analysis tools before \
         opening anything, and open a file only far \
         enough to confirm a hit is real. Return locations with a one-line reason \
         each. Never paste file contents into your \
         answer — the caller will read what it needs from the locations you give it, and copying \
         content here defeats the point of asking you.",
        &[
            "lean_analyze_workspace",
            "lean_doc_search",
            "lean_pdf_read_outline",
            "lean_word_read_outline",
            "lean_file_read",
        ],
        answer_schema(props(vec![(
            "hits",
            json!({
                "type": "array",
                "description": "Locations, most relevant first.",
                "items": {
                    "type": "object",
                    "properties": {
                        "path_or_document_id": {"type": "string"},
                        "locator": {"type": "string", "description": "Line range, page, or section."},
                        "why": {"type": "string", "description": "One line on why this matches."}
                    },
                    "required": ["path_or_document_id", "locator", "why"],
                    "additionalProperties": false
                }
            }),
        )])),
    )
}

pub fn extractor() -> Specialist {
    builtin_with_fan_out(
        "extractor",
        "Pulls the same named fields out of every document in a set and returns them as rows. \
         Use for structured extraction across many files — parties, dates, amounts, terms. Say \
         which fields you want in the request. Returns one row per document.",
        "You extract named fields from documents. The request names the fields; produce one row \
         per document with exactly those fields, in the same order every time. When a field is \
         genuinely absent from a document, return an empty value for it and say so in `notes` — \
         do not infer, do not carry a value over from another document, and do not omit the row.",
        &[
            "lean_doc_search",
            "lean_doc_read_chunk",
            "lean_file_read",
            "lean_pdf_read_text",
            "lean_pdf_read_outline",
            "lean_word_read_text",
            "lean_word_read_outline",
            "lean_excel_inspect",
            "lean_excel_read_rows",
        ],
        answer_schema(props(vec![(
            "rows",
            json!({
                "type": "array",
                "description": "One entry per source document.",
                "items": {
                    "type": "object",
                    "properties": {
                        "source": {"type": "string", "description": "Document id or path this row came from."},
                        "fields": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": {"type": "string"},
                                    "value": {"type": "string", "description": "Empty string when the field is absent."}
                                },
                                "required": ["name", "value"],
                                "additionalProperties": false
                            }
                        }
                    },
                    "required": ["source", "fields"],
                    "additionalProperties": false
                }
            }),
        )])),
        Some("per_ref"),
    )
}

pub fn analyst() -> Specialist {
    builtin(
        "analyst",
        "Computes figures from spreadsheets or tabular data and explains how. Use for arithmetic, \
         aggregation, or statistics over data too large or too fiddly to do in your head. Returns \
         the figures, the method, and any caveats about the data.",
        "You compute figures from data and show your method. Inspect the shape of the data before \
         reading it in bulk, and use the Python tool for anything beyond trivial arithmetic — do \
         not do multi-step arithmetic in your head and report it as computed. State the method \
         plainly enough that the caller could reproduce it, and put anything that qualifies the \
         numbers — missing rows, ambiguous units, an assumption you had to make — in `caveats`.",
        &[
            "lean_excel_inspect",
            "lean_excel_read_rows",
            "lean_file_read",
            "wasm_python_run",
            "execute_math_python",
        ],
        answer_schema(props(vec![
            (
                "figures",
                json!({
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": {"type": "string"},
                            "value": {"type": "string"},
                            "units": {"type": "string", "description": "Empty string if dimensionless."}
                        },
                        "required": ["label", "value", "units"],
                        "additionalProperties": false
                    }
                }),
            ),
            (
                "method",
                json!({"type": "string", "description": "How the figures were computed."}),
            ),
            (
                "caveats",
                json!({"type": "array", "items": {"type": "string"}}),
            ),
        ])),
    )
}

/// Every built-in, in the order they are offered to the model.
pub fn all() -> Vec<Specialist> {
    let mut list = vec![researcher(), summarizer(), locator(), extractor(), analyst()];
    // The shared preamble is prepended here rather than repeated in each
    // definition, so a change to it cannot apply to four specialists and miss
    // the fifth.
    for s in &mut list {
        s.system_prompt = format!("{COMMON}\n\n{}", s.system_prompt);
    }
    list
}

/// Write any built-in that is not already in the database.
///
/// Never overwrites: a built-in the user has edited in place stays edited
/// across restarts. Adding a new built-in in a later release still lands,
/// because only that name is absent.
pub async fn seed_builtins(pool: &SqlitePool) -> Result<usize, crate::error::StorageError> {
    let mut written = 0;
    for spec in all() {
        if store::insert_builtin_if_absent(pool, &spec).await? {
            tracing::info!(specialist = %spec.name, "seeded built-in specialist");
            written += 1;
        }
    }
    Ok(written)
}

/// Tool names in `tool_allow` that no connected server provides.
///
/// Checked when a definition is written rather than when it runs: a typo in a
/// tool name is otherwise invisible until a delegate quietly does its job
/// without the one tool that mattered, and the model has no way to tell that
/// from the tool simply not helping.
pub fn unknown_tools(mcp: &crate::mcp::MCPManager, app_id: &str, tool_allow: &[String]) -> Vec<String> {
    let known: std::collections::HashSet<String> = mcp
        .list_tools_for_app(app_id)
        .into_iter()
        .map(|t| t.name)
        .collect();
    tool_allow
        .iter()
        .filter(|t| !known.contains(*t))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every built-in must satisfy OpenAI's `strict: true` subset, since
    /// `provider::schema::directive_for` sends hosted OpenAI schemas with that
    /// flag and the endpoint 400s a schema that does not. A missing
    /// `additionalProperties: false`, or a property absent from `required`, is
    /// therefore a broken specialist rather than a lax one.
    #[test]
    fn every_builtin_schema_is_strict_mode_clean() {
        fn check(node: &Value, path: &str) {
            let Some(obj) = node.as_object() else { return };
            if obj.get("type").and_then(Value::as_str) == Some("object") {
                assert_eq!(
                    obj.get("additionalProperties"),
                    Some(&json!(false)),
                    "{path} is an object without additionalProperties:false"
                );
                let props = obj
                    .get("properties")
                    .and_then(Value::as_object)
                    .unwrap_or_else(|| panic!("{path} has no properties"));
                let required: Vec<&str> = obj
                    .get("required")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                for key in props.keys() {
                    assert!(
                        required.contains(&key.as_str()),
                        "{path}.{key} is not in `required`"
                    );
                }
                for (key, child) in props {
                    check(child, &format!("{path}.{key}"));
                }
            }
            if let Some(items) = obj.get("items") {
                check(items, &format!("{path}[]"));
            }
        }

        for spec in all() {
            let schema = spec
                .response_schema
                .as_ref()
                .unwrap_or_else(|| panic!("{} has no response schema", spec.name));
            check(schema, &spec.name);
        }
    }

    /// The two fields that keep a delegate honest about what it could not do.
    #[test]
    fn every_builtin_reports_notes_and_refusals() {
        for spec in all() {
            let schema = spec.response_schema.clone().unwrap();
            let props = schema["properties"].as_object().unwrap();
            assert!(props.contains_key("notes"), "{} lacks notes", spec.name);
            assert!(
                props.contains_key("refusals"),
                "{} lacks refusals",
                spec.name
            );
        }
    }

    /// No built-in may carry a write-class tool. Under `auto_reject` these do
    /// not fail loudly — they produce a specialist that silently half-works.
    #[test]
    fn no_builtin_can_write() {
        for spec in all() {
            for tool in &spec.tool_allow {
                assert!(
                    !crate::agent::loop_::is_write_tool(tool),
                    "{} is allowed the write tool {tool}",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn builtin_ids_are_stable_and_prompts_carry_the_common_preamble() {
        for spec in all() {
            assert_eq!(spec.id, format!("builtin:{}", spec.name));
            assert!(spec.system_prompt.starts_with(COMMON));
            assert!(spec.builtin && spec.enabled);
            assert!(spec.app_id.is_none());
        }
    }
}
