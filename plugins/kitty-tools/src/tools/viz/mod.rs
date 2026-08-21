//! `generate_accessible_table` / `generate_accessible_svg` / `generate_accessible_chart`.
//!
//! Layout, escaping, and validation live in sibling modules; this file is
//! purely dispatch: turn already-parsed request data into the JSON envelope
//! `server.rs`'s `#[tool]` stubs return, and nothing else.
//!
//! Every user-supplied string reaches SVG/HTML markup only through
//! `render::svg`/`render::table`'s escaping primitives — this file (and every
//! layout module) never `format!`s raw user text into markup directly. That
//! replaces this module's old "explicitly not fixed: no HTML escaping"
//! policy, which relied on the sandboxed iframe's opaque origin as the only
//! backstop; the guarantee now lives in the render API surface itself.

pub mod escape;
pub mod layout;
pub mod mermaid;
pub mod model;
pub mod render;
pub mod text;

use crate::envelope::error_response;
use serde_json::{json, Map, Value};

const WRAPPER: &str = include_str!("assets/wrapper.html");
const DEFS: &str = include_str!("assets/defs.svg");

fn wrap_in_standalone_html(title: &str, body_content: &str) -> String {
    // `TITLE` lands in the RCDATA `<title>…</title>` context, so it must be
    // HTML-escaped (`<`, `>`, `&`; quotes are inert there) — otherwise a
    // title like `</title><script>…` would close the element and execute.
    // `BODY` is already-escaped SVG/HTML from the renderers and must NOT be
    // double-escaped, so it passes through verbatim.
    let escaped_title = escape::escape_text(title);
    escape::render_template(
        WRAPPER,
        &[("TITLE", &escaped_title), ("BODY", body_content)],
    )
}

fn success_payload(title: &str, html_payload: &str, warnings: &[String]) -> String {
    let mut payload = Map::new();
    payload.insert("status".to_string(), json!("success"));
    payload.insert(
        "render_config".to_string(),
        json!({"target": "iframe", "title": title, "sandbox": "allow-scripts"}),
    );
    payload.insert("html_payload".to_string(), json!(html_payload));
    if !warnings.is_empty() {
        payload.insert("warnings".to_string(), json!(warnings));
    }
    serde_json::to_string_pretty(&Value::Object(payload)).unwrap_or_else(|_| "{}".to_string())
}

pub fn generate_accessible_table(
    title: &str,
    headers: &[String],
    rows: &[Vec<Value>],
    summary: Option<&str>,
) -> String {
    if let Err(e) = model::validate_table(headers, rows) {
        return e;
    }
    let fragment = render::table::render(title, headers, rows, summary);
    let standalone = wrap_in_standalone_html(title, &fragment);
    success_payload(title, &standalone, &[])
}

pub fn generate_accessible_svg(
    diagram_type: model::DiagramType,
    title: &str,
    description: &str,
    steps: Vec<model::Step>,
) -> String {
    let validated = match model::validate_diagram(diagram_type, title, description, steps) {
        Ok(v) => v,
        Err(e) => return e,
    };

    let (body, width, height) = match validated.diagram_type {
        model::DiagramType::SingleLane => layout::linear::render(&validated.steps),
        model::DiagramType::Flowchart => layout::graph::render_flowchart(&validated.steps),
        model::DiagramType::Tree => layout::graph::render_tree(&validated.steps),
        model::DiagramType::Swimlane => layout::swimlane::render(&validated.steps),
        model::DiagramType::JourneyMap => layout::journey::render(&validated.steps),
    };

    // Readability budget: layouts compress to fit, but anything still over
    // (e.g. an un-compressible swimlane/journey past their tighter node caps)
    // would render illegibly small when the iframe scales it to its own width —
    // better a helpful error than an unreadable diagram.
    let budget = match validated.diagram_type {
        model::DiagramType::Swimlane | model::DiagramType::JourneyMap => layout::MAX_CONTENT_W_WIDE,
        _ => layout::MAX_CONTENT_W,
    };
    if width > budget + layout::WIDTH_SLACK {
        return error_response(
            "VIZ_TOO_WIDE",
            &format!("This diagram is {width:.0}px wide, wider than the {budget:.0}px readability budget, so it would render illegibly small in the chat."),
            None,
            Some("Reduce the number of steps or nodes, or split the diagram into smaller ones."),
        );
    }

    let svg = render::svg::document(
        DEFS,
        &validated.title,
        &validated.description,
        width,
        height,
        &body,
    );
    let standalone = wrap_in_standalone_html(&validated.title, &svg);
    success_payload(&validated.title, &standalone, &validated.warnings)
}

#[allow(clippy::too_many_arguments)]
pub fn generate_accessible_chart(
    chart_type: model::ChartType,
    title: &str,
    description: &str,
    categories: Vec<String>,
    series: Vec<model::ChartSeries>,
    x_label: Option<&str>,
    y_label: Option<&str>,
) -> String {
    if let Err(e) = model::validate_chart(&categories, &series) {
        return e;
    }

    let (body, width, height) =
        layout::chart::render(chart_type, &categories, &series, x_label, y_label);
    let svg = render::svg::document(DEFS, title, description, width, height, &body);

    // A hidden data table alongside the SVG gives screen readers the exact
    // numbers a chart otherwise only conveys visually (bar height, line
    // slope) — nearly free since `render::table` already exists.
    let table_headers: Vec<String> = std::iter::once("Category".to_string())
        .chain(series.iter().map(|s| s.name.clone()))
        .collect();
    let table_rows: Vec<Vec<Value>> = categories
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut row = vec![json!(c)];
            row.extend(series.iter().map(|s| json!(s.values[i])));
            row
        })
        .collect();
    let sr_table = render::table::render_sr_only(title, &table_headers, &table_rows);

    let combined = format!("{svg}\n{sr_table}");
    let standalone = wrap_in_standalone_html(title, &combined);
    success_payload(title, &standalone, &[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{ChartSeries, ChartType, DiagramType, Step};

    #[test]
    fn table_success_payload_has_no_data_nesting() {
        let s = generate_accessible_table("T", &["A".to_string()], &[vec![json!(1)]], None);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["render_config"]["target"], "iframe");
        assert!(v["html_payload"].as_str().unwrap().contains("<table"));
    }

    #[test]
    fn table_error_uses_envelope_shape() {
        let s = generate_accessible_table("T", &[], &[], None);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["status"], "error");
        assert!(v["error_code"].is_string());
        assert!(v["hint"].is_string());
    }

    #[test]
    fn svg_success_carries_warnings_when_present() {
        let steps = vec![Step {
            text: "A".to_string(),
            sentiment: Some(1),
            ..Default::default()
        }];
        let s = generate_accessible_svg(DiagramType::SingleLane, "T", "D", steps);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["status"], "success");
        assert!(v["warnings"].as_array().is_some_and(|a| !a.is_empty()));
    }

    #[test]
    fn svg_error_never_uses_the_old_bare_shape() {
        // No diagram_type-level error exists anymore (invalid values are now
        // rejected earlier, by the wire enum) -- but an empty-steps request
        // still must produce the full envelope, not `{"status","message"}`.
        let s = generate_accessible_svg(DiagramType::Flowchart, "T", "D", vec![]);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["status"], "error");
        assert!(v["error_code"].is_string());
        assert!(v["hint"].is_string());
    }

    #[test]
    fn chart_success_embeds_a_screen_reader_table() {
        let categories = vec!["Q1".to_string(), "Q2".to_string()];
        let series = vec![ChartSeries {
            name: "Revenue".to_string(),
            values: vec![1.0, 2.0],
        }];
        let s = generate_accessible_chart(ChartType::Bar, "T", "D", categories, series, None, None);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["status"], "success");
        let html = v["html_payload"].as_str().unwrap();
        assert!(html.contains(r#"class="sr-only""#));
        assert!(html.contains("Revenue"));
    }

    #[test]
    fn title_containing_body_token_does_not_splice_body_content() {
        // Historical bug: `.replace("__TITLE__", t).replace("__BODY__", b)`
        // would let a title containing the literal "__BODY__" get re-scanned
        // by the second replace. `render_template` fixes this by construction.
        let steps = vec![Step {
            text: "Alpha".to_string(),
            ..Default::default()
        }];
        let s =
            generate_accessible_svg(DiagramType::SingleLane, "evil __BODY__ literal", "D", steps);
        let v: Value = serde_json::from_str(&s).unwrap();
        let html = v["html_payload"].as_str().unwrap();
        assert!(html.contains("evil __BODY__ literal"));
        assert!(
            html.contains("Alpha"),
            "the real body must still be present, not replaced by the title's literal token"
        );
    }

    #[test]
    fn title_containing_closing_tag_cannot_inject_script() {
        // Historical XSS: the title was interpolated into the wrapper unescaped
        // (`<title>__TITLE__</title>`), so a `</title><script>…` title escaped
        // the element and executed — the `script-src 'unsafe-inline'` CSP made
        // it worse. The title is now HTML-escaped before substitution.
        let s = generate_accessible_table(
            "</title><script>alert(1)</script>",
            &["A".to_string()],
            &[vec![json!(1)]],
            None,
        );
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["status"], "success");
        let html = v["html_payload"].as_str().unwrap();
        assert!(
            !html.contains("</title><script"),
            "title must not break out of the element: {html}"
        );
        assert!(
            html.contains("&lt;/title&gt;&lt;script&gt;alert(1)&lt;/script&gt;"),
            "title should appear HTML-escaped: {html}"
        );
    }
}
