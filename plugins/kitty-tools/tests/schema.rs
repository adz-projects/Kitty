//! Ratchet against the regression this whole viz rebuild was fixing: the
//! Rust port of the visualization tools silently dropped every per-field
//! JSON-Schema `description` the Python original carried, leaving a 27B
//! local model with almost no instruction surface — only `diagram_type` had
//! a doc comment. This test fails the build the moment a future edit adds an
//! undocumented field to any viz request type, or lets an enum regress from
//! a flat `{"type":"string","enum":[...]}` (which grammar-constrained
//! decoders like llama.cpp/Ollama handle reliably) to schemars' alternate
//! `oneOf`-of-`const` shape (which they handle far less reliably) — that
//! second shape is what schemars emits the moment any *variant* of a unit
//! enum picks up its own doc comment, so all per-value guidance in this
//! crate lives on the *field* that uses the enum instead.

use kitty_tools::server::{AccessibleChartRequest, AccessibleSvgRequest, AccessibleTableRequest};
use serde_json::Value;

const MIN_DESCRIPTION_LEN: usize = 30;

fn schema_value<T: schemars::JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap()
}

/// Every property on the root object, plus every property of every
/// object-shaped entry under `$defs` (nested param types like
/// `VizStepParam`/`ChartSeriesParam`), must carry a `description` of at
/// least `MIN_DESCRIPTION_LEN` characters.
fn assert_every_property_documented(schema: &Value, type_name: &str) {
    assert_properties_documented(schema, type_name);
    if let Some(defs) = schema.get("$defs").and_then(Value::as_object) {
        for (def_name, def_schema) in defs {
            if def_schema.get("properties").is_some() {
                assert_properties_documented(def_schema, &format!("{type_name}::{def_name}"));
            }
        }
    }
}

fn assert_properties_documented(schema: &Value, context: &str) {
    let properties = schema.get("properties").and_then(Value::as_object).unwrap_or_else(|| panic!("{context} has no `properties`"));
    for (field, field_schema) in properties {
        let description = field_schema
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{context}.{field} has no `description` at all -- a 27B model gets zero guidance on this field"));
        assert!(
            description.len() >= MIN_DESCRIPTION_LEN,
            "{context}.{field}'s description is only {} chars (\"{description}\"), below the {MIN_DESCRIPTION_LEN}-char floor",
            description.len()
        );
    }
}

fn assert_enum_defs_are_flat(schema: &Value, expected: &[(&str, &[&str])]) {
    let defs = schema.get("$defs").and_then(Value::as_object).expect("schema has no $defs");
    for (def_name, expected_values) in expected {
        let def = defs.get(*def_name).unwrap_or_else(|| panic!("missing $defs.{def_name}"));
        assert_eq!(def.get("type").and_then(Value::as_str), Some("string"), "$defs.{def_name} must be a flat string enum, not oneOf");
        assert!(def.get("oneOf").is_none(), "$defs.{def_name} regressed to oneOf-of-const -- a variant must have picked up a doc comment");
        let actual: Vec<&str> = def
            .get("enum")
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("$defs.{def_name} has no flat `enum`"))
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(&actual, expected_values, "$defs.{def_name} enum values changed");
    }
}

#[test]
fn svg_request_schema_is_fully_documented() {
    let schema = schema_value::<AccessibleSvgRequest>();
    assert_every_property_documented(&schema, "AccessibleSvgRequest");

    let required: Vec<&str> = schema["required"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(required, vec!["diagram_type", "title", "description", "steps"]);

    assert_enum_defs_are_flat(
        &schema,
        &[
            ("VizDiagramType", &["single_lane", "flowchart", "tree", "swimlane", "journey_map"]),
            ("VizStepType", &["start", "process", "decision", "end"]),
        ],
    );
}

#[test]
fn chart_request_schema_is_fully_documented() {
    let schema = schema_value::<AccessibleChartRequest>();
    assert_every_property_documented(&schema, "AccessibleChartRequest");

    let required: Vec<&str> = schema["required"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(required, vec!["chart_type", "title", "description", "categories", "series"]);

    assert_enum_defs_are_flat(&schema, &[("VizChartType", &["bar", "horizontal_bar", "line", "grouped_bar"])]);
}

#[test]
fn table_request_schema_is_fully_documented() {
    let schema = schema_value::<AccessibleTableRequest>();
    assert_every_property_documented(&schema, "AccessibleTableRequest");

    let required: Vec<&str> = schema["required"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(required, vec!["title", "headers", "rows"]);
}

/// Walk every schema position reachable from `schema` and collect the paths
/// of any that are a bare boolean rather than an object.
fn boolean_subschema_paths(schema: &Value, path: &str, out: &mut Vec<String>) {
    if schema.is_boolean() {
        out.push(path.to_string());
        return;
    }
    let Some(obj) = schema.as_object() else { return };
    for kw in ["items", "additionalItems", "contains", "propertyNames", "not", "additionalProperties", "unevaluatedProperties", "unevaluatedItems"] {
        if let Some(v) = obj.get(kw) {
            // `additionalProperties: false` is the standard "no extra
            // parameters" marker, understood everywhere; only `true` is the
            // problem shape.
            if (kw == "additionalProperties" || kw == "unevaluatedProperties") && v == &Value::Bool(false) {
                continue;
            }
            boolean_subschema_paths(v, &format!("{path}.{kw}"), out);
        }
    }
    for kw in ["properties", "patternProperties", "$defs", "definitions"] {
        if let Some(map) = obj.get(kw).and_then(Value::as_object) {
            for (name, v) in map {
                boolean_subschema_paths(v, &format!("{path}.{kw}.{name}"), out);
            }
        }
    }
    for kw in ["anyOf", "allOf", "oneOf", "prefixItems"] {
        if let Some(list) = obj.get(kw).and_then(Value::as_array) {
            for (i, v) in list.iter().enumerate() {
                boolean_subschema_paths(v, &format!("{path}.{kw}[{i}]"), out);
            }
        }
    }
}

/// Ratchet against the bug that made every llama-server request 400 with
/// `Unrecognized schema: true`: a `serde_json::Value` field (here, a table
/// cell) makes schemars emit the boolean schema `true`, and llama.cpp's
/// grammar builder rejects boolean sub-schemas outright — failing the whole
/// request, not just calls to the offending tool. Ollama tolerates them,
/// so this is invisible until someone points Kitty at llama.cpp.
#[test]
fn no_viz_schema_contains_a_boolean_subschema() {
    for (name, schema) in [
        ("AccessibleSvgRequest", schema_value::<AccessibleSvgRequest>()),
        ("AccessibleChartRequest", schema_value::<AccessibleChartRequest>()),
        ("AccessibleTableRequest", schema_value::<AccessibleTableRequest>()),
    ] {
        let mut found = Vec::new();
        boolean_subschema_paths(&schema, name, &mut found);
        assert!(
            found.is_empty(),
            "{name} has boolean sub-schemas at {found:?} -- llama.cpp will reject the entire tool list"
        );
    }
}
