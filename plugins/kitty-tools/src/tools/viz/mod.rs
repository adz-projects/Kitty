//! `generate_accessible_table` / `generate_accessible_svg` / `generate_accessible_chart`.
//!
//! Layout, escaping, and validation live in sibling modules; this file is
//! purely dispatch: turn already-parsed request data into the JSON envelope
//! `server.rs`'s `#[tool]` stubs return, and nothing else.
//!
//! Every user-supplied string reaches SVG/HTML markup only through
//! `render::svg`/`render::table`'s escaping primitives — this file (and every
//! layout module) never `format!`s raw user text into markup directly.
//!
//! # What the payload is
//!
//! A **fragment**, not a document: `<figure class="viz viz-{kind}">` holding a
//! `<figcaption>` title and the SVG or table. The host (Kitty's
//! `VisualizationCard`) sanitizes it and mounts it *inline* in the chat, in a
//! shadow root with its own stylesheet (`src/themes/viz.css`).
//!
//! It used to be a standalone HTML document for a sandboxed iframe, with the
//! palette baked into a `<style>` block and a script that measured the body
//! and posted its height to the host. Both halves of that were the product's
//! standing complaints: the frame's height handshake raced the SVG's layout
//! (frames too short, or zero), and a frame cannot inherit the app's theme (a
//! white slab in a dark chat). Inline, there is no frame to size and the
//! theme's custom properties simply inherit.
//!
//! So the markup here carries **class names only** — no colours, no fonts, no
//! `<style>`, no scripts. `class_font_px` in `render::svg` and the constants in
//! `layout` mirror the stylesheet's font sizes; nothing else is shared.

pub use kitty_doc::tools::viz::escape;
pub mod layout;
pub mod mermaid;
pub mod model;
pub mod render;
pub mod text;

use crate::envelope::error_response;
use serde_json::{json, Map, Value};

/// Arrowhead for diagram connectors. `refX="10"` puts the tip exactly on the
/// path's end point, i.e. on the edge of the node it points at.
const DIAGRAM_DEFS: &str = r#"<defs><marker id="arrow" viewBox="0 0 10 10" refX="10" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M 0 1.5 L 10 5 L 0 8.5 z" class="arrow-head"/></marker></defs>"#;

/// Engraver's fills for chart series: line, dot and cross hatching, so series
/// stay distinguishable without colour. Ink and paper come from the host
/// stylesheet via the `pat-*` classes.
const CHART_DEFS: &str = r#"<defs><pattern id="hatch-a" width="5" height="5" patternUnits="userSpaceOnUse" patternTransform="rotate(45)"><rect width="5" height="5" class="pat-bg"/><line x1="0" y1="0" x2="0" y2="5" class="pat-line"/></pattern><pattern id="hatch-b" width="5" height="5" patternUnits="userSpaceOnUse"><rect width="5" height="5" class="pat-bg"/><circle cx="2.5" cy="2.5" r="1.1" class="pat-dot"/></pattern><pattern id="hatch-c" width="6" height="6" patternUnits="userSpaceOnUse"><rect width="6" height="6" class="pat-bg"/><path d="M0,0 L6,6 M6,0 L0,6" class="pat-cross"/></pattern></defs>"#;

/// Wraps rendered content in the figure the host mounts: a flush-left caption
/// (HTML, so a long title wraps instead of being clipped by a narrow canvas —
/// it used to be SVG text centred on the drawing) above the content. `kind` is
/// one of `chart`, `diagram`, `table`, `mermaid`.
pub(crate) fn figure(kind: &str, title: &str, content: &str) -> String {
    let caption = if title.trim().is_empty() {
        String::new()
    } else {
        format!(
            r#"<figcaption class="viz-title">{}</figcaption>"#,
            escape::escape_text(title)
        )
    };
    format!(r#"<figure class="viz viz-{kind}">{caption}{content}</figure>"#)
}

pub(crate) fn success_payload(
    kind: &str,
    title: &str,
    html_payload: &str,
    warnings: &[String],
) -> String {
    let mut payload = Map::new();
    payload.insert("status".to_string(), json!("success"));
    payload.insert(
        "render_config".to_string(),
        json!({"target": "inline", "kind": kind, "title": title}),
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
    let table = render::table::render(title, headers, rows, summary);
    success_payload("table", title, &figure("table", title, &table), &[])
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
    // would render illegibly small once the chat scales it down to its column —
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
        DIAGRAM_DEFS,
        &validated.title,
        &validated.description,
        width,
        height,
        &body,
    );
    success_payload(
        "diagram",
        &validated.title,
        &figure("diagram", &validated.title, &svg),
        &validated.warnings,
    )
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
    let svg = render::svg::document(CHART_DEFS, title, description, width, height, &body);

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

    let combined = format!("{svg}{sr_table}");
    success_payload("chart", title, &figure("chart", title, &combined), &[])
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
        assert_eq!(v["render_config"]["target"], "inline");
        assert_eq!(v["render_config"]["kind"], "table");
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
    fn title_cannot_break_out_of_the_caption() {
        // Historical XSS: the title was once interpolated into the wrapper
        // document unescaped. It is now the figure's caption and must arrive
        // there as text.
        let s = generate_accessible_table(
            "</figcaption><script>alert(1)</script>",
            &["A".to_string()],
            &[vec![json!(1)]],
            None,
        );
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["status"], "success");
        let html = v["html_payload"].as_str().unwrap();
        assert!(!html.contains("<script"), "title must stay text: {html}");
        assert!(html.contains("&lt;/figcaption&gt;&lt;script&gt;alert(1)&lt;/script&gt;"));
    }

    /// The payload is mounted inline in the app's own document, so it must be
    /// inert and unstyled: a fragment, with no script, no stylesheet and no
    /// colour of its own (the host themes it by class name).
    #[test]
    fn every_payload_is_an_inert_unstyled_fragment() {
        let steps = vec![
            Step {
                id: Some("a".to_string()),
                text: "Alpha".to_string(),
                step_type: model::StepType::Start,
                next: vec!["b".to_string()],
                ..Default::default()
            },
            Step {
                id: Some("b".to_string()),
                text: "Beta?".to_string(),
                step_type: model::StepType::Decision,
                subtitle: Some("Check".to_string()),
                ..Default::default()
            },
        ];
        let series = vec![
            ChartSeries {
                name: "One".to_string(),
                values: vec![1.0, -2.0],
            },
            ChartSeries {
                name: "Two".to_string(),
                values: vec![3.0, 4.0],
            },
        ];
        let cats = vec!["Q1".to_string(), "Q2".to_string()];
        let mut payloads = vec![
            generate_accessible_table("T", &["A".to_string()], &[vec![json!(1)]], Some("S")),
            generate_accessible_svg(DiagramType::Flowchart, "T", "D", steps),
        ];
        for chart_type in [
            ChartType::Bar,
            ChartType::GroupedBar,
            ChartType::Line,
            ChartType::HorizontalBar,
        ] {
            payloads.push(generate_accessible_chart(
                chart_type,
                "T",
                "D",
                cats.clone(),
                series.clone(),
                Some("x"),
                Some("y"),
            ));
        }
        for p in payloads {
            let v: Value = serde_json::from_str(&p).unwrap();
            assert_eq!(v["status"], "success", "{p}");
            let html = v["html_payload"].as_str().unwrap();
            assert!(html.starts_with("<figure class=\"viz viz-"), "{html}");
            assert!(html.ends_with("</figure>"));
            // The only `#` allowed is a reference to the fragment's own defs.
            let scrubbed = html.to_lowercase().replace("url(#", "url(");
            for banned in [
                "<script", "<style", "<html", "<body", " style=", "#", "rgb(",
            ] {
                assert!(
                    !scrubbed.contains(banned),
                    "payload must not contain {banned:?}: {html}"
                );
            }
        }
    }

    #[test]
    fn svg_carries_its_natural_size_so_the_host_never_has_to_measure() {
        let s = generate_accessible_chart(
            ChartType::Bar,
            "T",
            "D",
            vec!["A".to_string()],
            vec![ChartSeries {
                name: "S".to_string(),
                values: vec![1.0],
            }],
            None,
            None,
        );
        let v: Value = serde_json::from_str(&s).unwrap();
        let html = v["html_payload"].as_str().unwrap();
        let tag = &html[html.find("<svg").unwrap()..];
        let tag = &tag[..tag.find('>').unwrap()];
        let get = |name: &str| -> String {
            tag.split(&format!(r#" {name}=""#))
                .nth(1)
                .and_then(|r| r.split('"').next())
                .unwrap_or_default()
                .to_string()
        };
        let (w, h) = (get("width"), get("height"));
        assert!(w.parse::<f32>().unwrap() > 0.0 && h.parse::<f32>().unwrap() > 0.0);
        assert_eq!(get("viewBox"), format!("0 0 {w} {h}"));
    }
}
