//! Schema-constrained output, mapped per provider dialect.
//!
//! # Why
//!
//! Nothing in V1 touched `response_format`, JSON mode, or grammars. What it
//! *did* have was `agent::json_extract` — a module for recovering JSON from
//! prose after the fact, which is the tell: the daemon was already scraping
//! structure out of best-effort text.
//!
//! A pipeline extracting fields from five hundred documents needs a guarantee,
//! not a scrape. Every dialect can provide one; none of them agree on how.
//!
//! # The dialects
//!
//! | Dialect | Mechanism |
//! |---|---|
//! | OpenAI, OpenRouter | `response_format: {type: "json_schema", …}` |
//! | Anthropic, adaptive thinking | `output_config.format: {type: "json_schema", schema}` |
//! | Anthropic, older models | force a single tool whose `input_schema` is the schema, then unwrap the `tool_use` block |
//! | llama.cpp, Ollama | a GBNF grammar / the `format` field |
//!
//! Anthropic's older models have no `response_format`, and tool-forcing *is*
//! their structured-output mechanism rather than a workaround. The current
//! ones (Opus 5.5, Sonnet 5.5, Fable 5.1 — the ones a client marks
//! `anthropic_thinking: "adaptive"`) reject a forced `tool_choice` with a 400,
//! so they get structured outputs instead, which also lets the answer carry
//! thinking at the requested effort.
//!
//! # Interaction with tools
//!
//! On most dialects schema-constrained output and tool calling are mutually
//! exclusive — Anthropic's mechanism *is* a tool call, and OpenAI's
//! `json_schema` mode forbids parallel tool use. Rather than let each provider
//! fail differently, the rule is fixed here and documented: **the tool loop
//! runs normally, and the schema constrains only the final answer.** A turn
//! may therefore call tools and still return validated JSON.

use serde_json::{json, Value};

/// What a caller asked for.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum ResponseMode {
    /// Ordinary prose. The default.
    #[default]
    Text,
    /// Valid JSON, with no schema attached.
    Json,
    /// JSON matching a supplied schema.
    Schema,
}

/// What one request asks of the model's final answer, before it has been
/// mapped to a dialect.
///
/// Carried from the caller down to [`ProviderRouter::chat_completion`], which
/// is the only layer that knows which dialect a provider speaks and therefore
/// the only one that can call [`directive_for`]. A caller states intent; the
/// router states wire shape.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ResponseSpec {
    #[serde(default)]
    pub mode: ResponseMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
}

impl ResponseSpec {
    /// The ordinary case: prose, no constraint.
    pub fn text() -> Self {
        Self::default()
    }

    /// Constrain the final answer to `schema`.
    pub fn schema(schema: Value) -> Self {
        Self {
            mode: ResponseMode::Schema,
            schema: Some(schema),
        }
    }

    /// Whether this spec asks for anything at all. Lets callers skip the
    /// validate/extract path entirely on an ordinary turn.
    pub fn is_constrained(&self) -> bool {
        !matches!(self.mode, ResponseMode::Text)
    }
}

/// The name given to Anthropic's forced tool.
///
/// Arbitrary but fixed: the unwrap step has to find the block again, and a
/// name the model might plausibly produce on its own would be ambiguous.
pub const ANTHROPIC_STRUCTURED_TOOL: &str = "__structured_response";

/// How a request should be shaped for one dialect.
#[derive(Debug, Clone, PartialEq)]
pub enum SchemaDirective {
    /// Nothing to add.
    None,
    /// OpenAI-style `response_format`.
    ResponseFormat(Value),
    /// Anthropic-style forced tool: `(tool definition, tool_choice)`.
    ForcedTool(Value, Value),
    /// Anthropic structured outputs: the `output_config.format` object.
    OutputFormat(Value),
    /// llama.cpp / Ollama `format` field.
    Format(Value),
}

/// Build the dialect-specific directive for a mode and schema.
///
/// Returns [`SchemaDirective::None`] for `Text`, and for `Schema` with no
/// schema supplied — a caller who asks for schema mode without one gets
/// unconstrained output rather than an error, because the alternative is
/// failing a turn over a request shape the caller can fix by reading the
/// response.
pub fn directive_for(
    dialect: &str,
    mode: &ResponseMode,
    schema: Option<&Value>,
) -> SchemaDirective {
    match mode {
        ResponseMode::Text => SchemaDirective::None,

        ResponseMode::Json => match dialect {
            "anthropic" => SchemaDirective::None, // no JSON mode without a schema
            "ollama" | "custom_openai" | "local" => SchemaDirective::Format(json!("json")),
            _ => SchemaDirective::ResponseFormat(json!({"type": "json_object"})),
        },

        ResponseMode::Schema => {
            let Some(schema) = schema else {
                return SchemaDirective::None;
            };
            match dialect {
                "anthropic" => SchemaDirective::ForcedTool(
                    json!({
                        "name": ANTHROPIC_STRUCTURED_TOOL,
                        "description": "Return the response in the required structure.",
                        "input_schema": schema,
                    }),
                    json!({"type": "tool", "name": ANTHROPIC_STRUCTURED_TOOL}),
                ),
                // Self-hosted servers take a raw JSON Schema in `format`,
                // which they compile to a GBNF grammar internally.
                "ollama" | "custom_openai" | "local" => SchemaDirective::Format(schema.clone()),
                _ => SchemaDirective::ResponseFormat(json!({
                    "type": "json_schema",
                    "json_schema": {
                        "name": "response",
                        // Without this OpenAI treats the schema as advisory,
                        // which defeats the point of asking for a guarantee.
                        "strict": true,
                        "schema": schema,
                    }
                })),
            }
        }
    }
}

/// The directive for an Anthropic model on adaptive thinking.
///
/// Structured outputs rather than a forced tool: these models 400 on
/// `tool_choice` `any`/`tool`, which made every schema'd delegate on them fail
/// its final pass. The schema is narrowed to what structured outputs accept
/// (see [`for_structured_outputs`]); the answer is still validated against the
/// caller's full schema afterwards, so nothing the caller asked for is lost.
pub fn directive_for_adaptive_anthropic(
    mode: &ResponseMode,
    schema: Option<&Value>,
) -> SchemaDirective {
    match (mode, schema) {
        (ResponseMode::Schema, Some(schema)) => SchemaDirective::OutputFormat(json!({
            "type": "json_schema",
            "schema": for_structured_outputs(schema),
        })),
        _ => directive_for("anthropic", mode, schema),
    }
}

/// A schema reshaped for Anthropic structured outputs, which require
/// `additionalProperties: false` on every object and reject numeric, string
/// length and array-size constraints. Adds the first and drops the rest — the
/// same thing the official SDKs do before sending, and for the same reason:
/// [`validate`] still enforces the dropped constraints on the answer.
pub fn for_structured_outputs(schema: &Value) -> Value {
    const UNSUPPORTED: &[&str] = &[
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "minLength",
        "maxLength",
        "maxItems",
        "uniqueItems",
        "minProperties",
        "maxProperties",
    ];
    match schema {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if UNSUPPORTED.contains(&k.as_str()) {
                    continue;
                }
                // `minItems` above 1 is rejected; 0 and 1 are accepted.
                if k == "minItems" && v.as_u64().is_some_and(|n| n > 1) {
                    continue;
                }
                // A map of *names* to subschemas: a property may be called
                // `maximum`, so its keys are names, never constraints.
                let narrowed = match (k.as_str(), v) {
                    ("properties" | "$defs" | "definitions", Value::Object(named)) => {
                        Value::Object(
                            named
                                .iter()
                                .map(|(name, sub)| (name.clone(), for_structured_outputs(sub)))
                                .collect(),
                        )
                    }
                    _ => for_structured_outputs(v),
                };
                out.insert(k.clone(), narrowed);
            }
            let is_object = out.get("type").is_some_and(|t| {
                t == "object"
                    || t.as_array()
                        .is_some_and(|a| a.iter().any(|x| x == "object"))
            });
            if is_object && !matches!(out.get("additionalProperties"), Some(Value::Bool(false))) {
                out.insert("additionalProperties".into(), Value::Bool(false));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(for_structured_outputs).collect()),
        other => other.clone(),
    }
}

/// Validate a model's answer against the requested schema.
///
/// Reuses `mcp::tools`' compiled-validator cache rather than compiling here:
/// a pipeline sends the same schema thousands of times, and compiling it per
/// request would be the dominant cost of a small extraction.
pub fn validate(schema: &Value, answer: &Value) -> Result<(), String> {
    let Some(validator) = crate::mcp::tools::validator_for(schema) else {
        // An uncompilable schema is the caller's bug, but failing the turn over
        // it would discard a completed generation. Accept and say so.
        tracing::warn!("response schema did not compile; skipping validation");
        return Ok(());
    };
    let errors: Vec<String> = validator
        .iter_errors(answer)
        .map(|e| format!("{}: {e}", e.instance_path))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Pull the structured answer out of a completed turn's text.
///
/// Anthropic returns it as a `tool_use` block, which the loop surfaces as tool
/// arguments; everything else returns it as the message body. Falls back to
/// `agent::json_extract`, which handles a model that wrapped its JSON in a
/// code fence or prose despite being told not to.
pub fn extract(text: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
        return Some(v);
    }
    crate::agent::json_extract::extract_json(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {"title": {"type": "string"}},
            "required": ["title"],
        })
    }

    #[test]
    fn openai_gets_a_strict_json_schema_response_format() {
        let d = directive_for("openai", &ResponseMode::Schema, Some(&schema()));
        let SchemaDirective::ResponseFormat(v) = d else {
            panic!("expected response_format, got {d:?}");
        };
        assert_eq!(v["type"], "json_schema");
        // Without `strict` the schema is advisory, which defeats the point.
        assert_eq!(v["json_schema"]["strict"], true);
    }

    #[test]
    fn anthropic_gets_a_forced_tool_because_it_has_no_response_format() {
        // Tool-forcing *is* Anthropic's structured-output mechanism, not a
        // workaround for the absence of one.
        let d = directive_for("anthropic", &ResponseMode::Schema, Some(&schema()));
        let SchemaDirective::ForcedTool(tool, choice) = d else {
            panic!("expected a forced tool, got {d:?}");
        };
        assert_eq!(tool["name"], ANTHROPIC_STRUCTURED_TOOL);
        assert_eq!(tool["input_schema"], schema());
        assert_eq!(choice["type"], "tool");
        assert_eq!(choice["name"], ANTHROPIC_STRUCTURED_TOOL);
    }

    /// Current Claude models 400 on a forced `tool_choice`, so an adaptive
    /// Anthropic provider gets structured outputs — the shape the Messages API
    /// documents: `output_config.format = {type: "json_schema", schema}`.
    #[test]
    fn adaptive_anthropic_gets_structured_outputs_not_a_forced_tool() {
        let d = directive_for_adaptive_anthropic(&ResponseMode::Schema, Some(&schema()));
        let SchemaDirective::OutputFormat(f) = d else {
            panic!("expected output_config.format, got {d:?}");
        };
        assert_eq!(f["type"], "json_schema");
        assert_eq!(f["schema"]["properties"], schema()["properties"]);
        assert_eq!(f["schema"]["additionalProperties"], false);
        // Text is still nothing at all.
        assert_eq!(
            directive_for_adaptive_anthropic(&ResponseMode::Text, Some(&schema())),
            SchemaDirective::None
        );
    }

    /// Narrowed for the wire only: unsupported constraints go, every object
    /// is closed, and the rest of the schema is untouched.
    #[test]
    fn structured_outputs_schema_drops_unsupported_constraints() {
        let s = json!({
            "type": "object",
            "properties": {
                "n": {"type": "integer", "minimum": 0, "maximum": 9},
                "maximum": {"type": "string"},
                "s": {"type": "string", "maxLength": 20, "enum": ["a", "b"]},
                "list": {"type": "array", "minItems": 3, "items": {
                    "type": "object", "properties": {"x": {"type": "string"}}
                }},
            },
            "required": ["n"],
            "additionalProperties": true,
        });
        let out = for_structured_outputs(&s);
        assert_eq!(out["additionalProperties"], false);
        assert!(out["properties"]["n"].get("minimum").is_none());
        assert!(out["properties"]["s"].get("maxLength").is_none());
        assert_eq!(out["properties"]["s"]["enum"], json!(["a", "b"]));
        assert!(out["properties"]["list"].get("minItems").is_none());
        assert_eq!(
            out["properties"]["list"]["items"]["additionalProperties"],
            false
        );
        assert_eq!(out["required"], json!(["n"]));
        // A property *named* like a constraint is a property.
        assert_eq!(out["properties"]["maximum"]["type"], "string");
    }

    #[test]
    fn self_hosted_servers_get_the_schema_in_format() {
        for dialect in ["ollama", "custom_openai", "local"] {
            let d = directive_for(dialect, &ResponseMode::Schema, Some(&schema()));
            assert_eq!(d, SchemaDirective::Format(schema()), "dialect {dialect}");
        }
    }

    #[test]
    fn json_mode_without_a_schema_maps_per_dialect() {
        assert_eq!(
            directive_for("openai", &ResponseMode::Json, None),
            SchemaDirective::ResponseFormat(json!({"type": "json_object"}))
        );
        assert_eq!(
            directive_for("ollama", &ResponseMode::Json, None),
            SchemaDirective::Format(json!("json"))
        );
        // Anthropic has no schema-less JSON mode; asking for one adds nothing
        // rather than sending a field it would reject.
        assert_eq!(
            directive_for("anthropic", &ResponseMode::Json, None),
            SchemaDirective::None
        );
    }

    #[test]
    fn text_mode_adds_nothing_anywhere() {
        for dialect in ["openai", "anthropic", "ollama", "custom_openai"] {
            assert_eq!(
                directive_for(dialect, &ResponseMode::Text, Some(&schema())),
                SchemaDirective::None,
                "dialect {dialect}"
            );
        }
    }

    #[test]
    fn schema_mode_with_no_schema_is_unconstrained_rather_than_an_error() {
        // Failing the turn would be a worse answer than one the caller can
        // diagnose from the response they get.
        assert_eq!(
            directive_for("openai", &ResponseMode::Schema, None),
            SchemaDirective::None
        );
    }

    #[test]
    fn validation_accepts_a_conforming_answer_and_names_what_is_wrong() {
        assert!(validate(&schema(), &json!({"title": "ok"})).is_ok());

        let err = validate(&schema(), &json!({"title": 7})).unwrap_err();
        assert!(!err.is_empty(), "a failure must say what was wrong");

        assert!(validate(&schema(), &json!({})).is_err(), "missing required");
    }

    #[test]
    fn an_uncompilable_schema_does_not_discard_a_finished_generation() {
        let bad = json!({"type": "not-a-real-type"});
        assert!(validate(&bad, &json!({"anything": true})).is_ok());
    }

    #[test]
    fn extraction_handles_bare_json_and_fenced_json() {
        assert_eq!(extract(r#"{"a":1}"#), Some(json!({"a": 1})));
        assert_eq!(
            extract("Here you go:\n```json\n{\"a\":1}\n```"),
            Some(json!({"a": 1}))
        );
        assert_eq!(extract("no json here at all"), None);
    }
}
