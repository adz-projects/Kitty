//! `generate_accessible_table`/`generate_accessible_svg` — Rust port of
//! `plugins/visualizations/visualizations.py`.
//!
//! The ~430 lines of literal HTML/CSS/SVG template are `include_str!`
//! assets (see `assets/`), not hand-transcribed Rust string literals — the
//! base plan calls this out specifically: Python f-strings double braces
//! (`{{` -> `{`), so hand-transcription invites one-character diffs. Storing
//! the *emitted* form and substituting placeholders with plain `.replace()`
//! avoids that class of bug entirely.
//!
//! **Explicitly not fixed here**: `title`/`headers`/`rows`/`summary`/
//! `description` are interpolated raw, with no HTML escaping — matching the
//! Python original. The base plan judges the risk bounded (the iframe is
//! `sandbox="allow-scripts"` *without* `allow-same-origin`, an opaque
//! origin that can't reach Kitty's DOM or storage) and defers a fix.

use serde_json::{json, Value};

const WRAPPER: &str = include_str!("assets/wrapper.html");
const TABLE_CSS: &str = include_str!("assets/table.css");
const DEFS: &str = include_str!("assets/defs.svg");
const FLOWCHART: &str = include_str!("assets/flowchart.svg");
const SWIMLANE: &str = include_str!("assets/swimlane.svg");
const JOURNEY_MAP: &str = include_str!("assets/journey_map.svg");

fn wrap_in_standalone_html(title: &str, body_content: &str) -> String {
    WRAPPER.replace("__TITLE__", title).replace("__BODY__", body_content).trim().to_string()
}

fn render_table(title: &str, headers: &[String], rows: &[Vec<Value>], summary: Option<&str>) -> String {
    let summary_html = summary.filter(|s| !s.is_empty()).map(|s| format!(r#"<p class="sr-only">{s}</p>"#)).unwrap_or_default();
    let header_cells: String = headers.iter().map(|h| format!(r#"<th scope="col">{h}</th>"#)).collect();

    let body_rows: String = rows
        .iter()
        .map(|row| {
            let cells: String = row
                .iter()
                .enumerate()
                .map(|(idx, val)| {
                    let text = value_to_display(val);
                    if idx == 0 {
                        format!(r#"<th scope="row">{text}</th>"#)
                    } else {
                        format!("<td>{text}</td>")
                    }
                })
                .collect();
            format!("<tr>{cells}</tr>")
        })
        .collect();

    format!(
        r#"<div class="mcp-table-wrapper">
    {TABLE_CSS}
    {summary_html}
    <table class="mcp-grayscale-table">
        <caption>{title}</caption>
        <thead>
            <tr>{header_cells}</tr>
        </thead>
        <tbody>
            {body_rows}
        </tbody>
    </table>
</div>"#
    )
    .trim()
    .to_string()
}

fn value_to_display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub fn generate_accessible_table(title: &str, headers: &[String], rows: &[Vec<Value>], summary: Option<&str>) -> String {
    let fragment = render_table(title, headers, rows, summary);
    let standalone = wrap_in_standalone_html(title, &fragment);
    let payload = json!({
        "status": "success",
        "render_config": {"target": "iframe", "title": title, "sandbox": "allow-scripts"},
        "html_payload": standalone,
    });
    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string())
}

#[derive(Debug, Clone, Default)]
pub struct VizStep {
    pub text: String,
    pub step_type: String,
    pub subtitle: Option<String>,
}

fn render_single_lane_process(title: &str, description: &str, steps: &[VizStep]) -> String {
    let svg_width = 880;
    let svg_height = 220;
    let mut step_elements = String::new();
    let mut arrow_elements = String::new();
    let mut x_offset = 30;
    let y_center = 115;

    for (i, step) in steps.iter().enumerate() {
        let stype = step.step_type.to_lowercase();
        let label = if step.text.is_empty() { format!("Step {}", i + 1) } else { step.text.clone() };

        let node_width = if stype == "gate" {
            let sub = step.subtitle.clone().unwrap_or_else(|| "GATE".to_string());
            let text_content = if label.chars().count() > 13 && label.contains(' ') {
                let mut parts = label.splitn(2, ' ');
                let p1 = parts.next().unwrap_or_default();
                let p2 = parts.next().unwrap_or_default();
                format!(r#"<text x="65" y="44" class="node-text" style="font-size:10px;"><tspan x="65" dy="0">{p1}</tspan><tspan x="65" dy="12">{p2}</tspan></text>"#)
            } else {
                format!(r#"<text x="65" y="48" class="node-text" style="font-size:11px;">{label}</text>"#)
            };
            step_elements.push_str(&format!(
                r#"
                <g transform="translate({x_offset}, 80)">
                    <polygon points="65,0 130,70 0,70" class="node-triangle"/>
                    <text x="65" y="24" class="badge-meta">{sub}</text>
                    {text_content}
                </g>
                "#
            ));
            130
        } else {
            step_elements.push_str(&format!(
                r#"
                <g transform="translate({x_offset}, 90)">
                    <rect width="130" height="50" class="node-box"/>
                    <text x="65" y="25" class="node-text">{label}</text>
                </g>
                "#
            ));
            130
        };

        if i < steps.len() - 1 {
            let next_x = x_offset + node_width + 40;
            arrow_elements.push_str(&format!(
                r#"<line x1="{}" y1="{y_center}" x2="{next_x}" y2="{y_center}" class="flow-path"/>"#,
                x_offset + node_width
            ));
            x_offset = next_x;
        }
    }

    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {svg_width} {svg_height}" width="100%" height="auto" role="img" aria-label="{title}">
    <title>{title}</title>
    <desc>{description}</desc>
    {DEFS}

    <rect width="100%" height="100%" class="canvas-bg"/>
    <text x="440" y="35" class="title-text">{title}</text>
    {arrow_elements}
    {step_elements}
</svg>"#
    )
    .trim()
    .to_string()
}

fn default_single_lane_steps() -> Vec<VizStep> {
    vec![
        VizStep { text: "1. Ingest Data".into(), step_type: "process".into(), subtitle: None },
        VizStep { text: "2. Schema Valid?".into(), step_type: "gate".into(), subtitle: Some("GATE".into()) },
        VizStep { text: "3. Transform".into(), step_type: "process".into(), subtitle: None },
        VizStep { text: "4. Security Audit?".into(), step_type: "gate".into(), subtitle: Some("AUDIT".into()) },
        VizStep { text: "5. Publish Event".into(), step_type: "process".into(), subtitle: None },
    ]
}

pub fn generate_accessible_svg(diagram_type: &str, title: &str, description: &str, steps: Option<&[VizStep]>) -> String {
    let dtype = diagram_type.to_lowercase();
    let svg_raw = match dtype.as_str() {
        "flowchart" => FLOWCHART.replace("__TITLE__", title).replace("__DESCRIPTION__", description).replace("__DEFS__", DEFS).trim().to_string(),
        "single_lane" => {
            let owned;
            let steps = match steps {
                Some(s) if !s.is_empty() => s,
                _ => {
                    owned = default_single_lane_steps();
                    &owned
                }
            };
            render_single_lane_process(title, description, steps)
        }
        "swimlane" => SWIMLANE.replace("__TITLE__", title).replace("__DESCRIPTION__", description).replace("__DEFS__", DEFS).trim().to_string(),
        "journey_map" => JOURNEY_MAP.replace("__TITLE__", title).replace("__DESCRIPTION__", description).replace("__DEFS__", DEFS).trim().to_string(),
        _ => {
            let payload = json!({
                "status": "error",
                "message": format!("Unsupported diagram_type '{diagram_type}'. Choose from 'flowchart', 'single_lane', 'swimlane', or 'journey_map'."),
            });
            return serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
        }
    };

    let standalone = wrap_in_standalone_html(title, &svg_raw);
    let payload = json!({
        "status": "success",
        "render_config": {"target": "iframe", "title": title, "sandbox": "allow-scripts"},
        "html_payload": standalone,
    });
    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_renders_headers_and_rows() {
        let s = generate_accessible_table(
            "My Table",
            &["A".to_string(), "B".to_string()],
            &[vec![json!("1"), json!("2")]],
            None,
        );
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["status"], "success");
        let html = v["html_payload"].as_str().unwrap();
        assert!(html.contains("My Table"));
        assert!(html.contains(r#"<th scope="row">1</th>"#));
        assert!(html.contains("<td>2</td>"));
    }

    #[test]
    fn flowchart_substitutes_title_and_description() {
        let s = generate_accessible_svg("flowchart", "T", "D", None);
        let v: Value = serde_json::from_str(&s).unwrap();
        let html = v["html_payload"].as_str().unwrap();
        assert!(html.contains("<title>T</title>"));
        assert!(html.contains("<desc>D</desc>"));
        assert!(!html.contains("__TITLE__"));
        assert!(!html.contains("__DEFS__"));
    }

    #[test]
    fn unsupported_diagram_type_reports_error() {
        let s = generate_accessible_svg("bogus", "T", "D", None);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["status"], "error");
    }

    #[test]
    fn single_lane_uses_default_steps_when_none_given() {
        let s = generate_accessible_svg("single_lane", "T", "D", None);
        let v: Value = serde_json::from_str(&s).unwrap();
        let html = v["html_payload"].as_str().unwrap();
        assert!(html.contains("1. Ingest Data"));
        assert!(html.contains("GATE"));
    }
}
