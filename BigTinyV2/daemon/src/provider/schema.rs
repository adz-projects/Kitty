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
//! | Anthropic | force a single tool whose `input_schema` is the schema, then unwrap the `tool_use` block |
//! | llama.cpp, Ollama | a GBNF grammar / the `format` field |
//!
//! Anthropic's is the one that surprises people: it has no `response_format`,
//! and tool-forcing *is* its structured-output mechanism rather than a
//! workaround.
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
pub enum ResponseMode {
    /// Ordinary prose. The default.
    Text,
    /// Valid JSON, with no schema attached.
    Json,
    /// JSON matching a supplied schema.
    Schema,
}

impl Default for ResponseMode {
    fn default() -> Self {
        Self::Text
    }
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
pub fn directive_for(dialect: &str, mode: &ResponseMode, schema: Option<&Value>) -> SchemaDirective {
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
