//! HTML table rendering — the accessible-table half of the crate's two output
//! formats. Validation (ragged rows, empty inputs) happens in `model.rs`
//! before this is called; this module only escapes and renders.

use serde_json::Value;

use crate::tools::viz::escape::escape_text;

const TABLE_CSS: &str = include_str!("../assets/table.css");

pub fn render(
    title: &str,
    headers: &[String],
    rows: &[Vec<Value>],
    summary: Option<&str>,
) -> String {
    let summary_html = summary
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!(r#"<p class="sr-only">{}</p>"#, escape_text(s)))
        .unwrap_or_default();

    let header_cells: String = headers
        .iter()
        .map(|h| format!(r#"<th scope="col">{}</th>"#, escape_text(h)))
        .collect();

    let body_rows: String = rows
        .iter()
        .map(|row| {
            let cells: String = row
                .iter()
                .enumerate()
                .map(|(idx, val)| {
                    let text = escape_text(&value_to_display(val));
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
        <caption>{}</caption>
        <thead>
            <tr>{header_cells}</tr>
        </thead>
        <tbody>
            {body_rows}
        </tbody>
    </table>
</div>"#,
        escape_text(title)
    )
    .trim()
    .to_string()
}

/// Renders a plain `<table class="sr-only">` with no styling hooks other than
/// visual hiding — used by chart output to give screen readers the exact
/// numbers behind a chart the SVG only shows as bars/lines.
pub fn render_sr_only(title: &str, headers: &[String], rows: &[Vec<Value>]) -> String {
    let header_cells: String = headers
        .iter()
        .map(|h| format!(r#"<th scope="col">{}</th>"#, escape_text(h)))
        .collect();
    let body_rows: String = rows
        .iter()
        .map(|row| {
            let cells: String = row
                .iter()
                .enumerate()
                .map(|(idx, val)| {
                    let text = escape_text(&value_to_display(val));
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
        r#"<table class="sr-only"><caption>{}</caption><thead><tr>{header_cells}</tr></thead><tbody>{body_rows}</tbody></table>"#,
        escape_text(title)
    )
}

fn value_to_display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_headers_and_rows() {
        let html = render(
            "My Table",
            &["A".to_string(), "B".to_string()],
            &[vec![json!("1"), json!("2")]],
            None,
        );
        assert!(html.contains("My Table"));
        assert!(html.contains(r#"<th scope="row">1</th>"#));
        assert!(html.contains("<td>2</td>"));
    }

    #[test]
    fn escapes_hostile_cell_content() {
        let html = render(
            "T",
            &["H".to_string()],
            &[vec![json!("<script>alert(1)</script>")]],
            None,
        );
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn escapes_hostile_title_and_summary() {
        let html = render(
            "<b>T</b>",
            &["H".to_string()],
            &[vec![json!("x")]],
            Some("<i>S</i>"),
        );
        assert!(!html.contains("<b>"));
        assert!(!html.contains("<i>"));
    }

    #[test]
    fn null_cell_renders_empty_and_bool_renders_word() {
        let html = render(
            "T",
            &["A".to_string(), "B".to_string()],
            &[vec![json!(null), json!(true)]],
            None,
        );
        assert!(html.contains(r#"<th scope="row"></th>"#));
        assert!(html.contains("<td>true</td>"));
    }

    #[test]
    fn sr_only_table_carries_scope_row_on_first_column() {
        let html = render_sr_only(
            "Chart data",
            &["Category".to_string(), "Revenue".to_string()],
            &[vec![json!("Q1"), json!(12.4)]],
        );
        assert!(html.contains(r#"class="sr-only""#));
        assert!(html.contains(r#"<th scope="row">Q1</th>"#));
    }
}
