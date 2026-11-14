//! HTML table rendering — the accessible-table half of the crate's two output
//! formats. Validation (ragged rows, empty inputs) happens in `model.rs`
//! before this is called; this module only escapes and renders.
//!
//! The markup carries no styling of its own: the host stylesheet sets it as a
//! ruled table (horizontal rules only, figures ranged right). The one visual
//! decision made here is *which* columns are figures, since only this side
//! still knows a cell was a JSON number rather than a string of digits.

use serde_json::Value;

use crate::tools::viz::escape::{escape_attr, escape_text};

/// A column is set as figures (ranged right, tabular numerals) when every
/// non-empty cell in it is a JSON number and at least one is.
fn numeric_columns(width: usize, rows: &[Vec<Value>]) -> Vec<bool> {
    (0..width)
        .map(|col| {
            let mut any = false;
            for row in rows {
                match row.get(col) {
                    Some(Value::Number(_)) => any = true,
                    Some(Value::Null) | None => {}
                    Some(_) => return false,
                }
            }
            any
        })
        .collect()
}

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

    let numeric = numeric_columns(headers.len(), rows);
    let num_class = |idx: usize| -> &'static str {
        if numeric.get(idx).copied().unwrap_or(false) {
            r#" class="num""#
        } else {
            ""
        }
    };

    let header_cells: String = headers
        .iter()
        .enumerate()
        .map(|(idx, h)| {
            format!(
                r#"<th scope="col"{}>{}</th>"#,
                num_class(idx),
                escape_text(h)
            )
        })
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
                        format!(r#"<th scope="row"{}>{text}</th>"#, num_class(idx))
                    } else {
                        format!("<td{}>{text}</td>", num_class(idx))
                    }
                })
                .collect();
            format!("<tr>{cells}</tr>")
        })
        .collect();

    // The visible title is the figure's caption (see `viz::figure`), so the
    // table names itself to assistive technology with `aria-label` instead of
    // repeating it in a `<caption>`.
    format!(
        r#"{summary_html}<div class="viz-table-wrap"><table class="viz-table" aria-label="{}"><thead><tr>{header_cells}</tr></thead><tbody>{body_rows}</tbody></table></div>"#,
        escape_attr(title)
    )
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
        assert!(html.contains(r#"aria-label="My Table""#));
        assert!(html.contains(r#"<th scope="row">1</th>"#));
        assert!(html.contains("<td>2</td>"));
    }

    #[test]
    fn columns_of_json_numbers_are_marked_as_figures() {
        let html = render(
            "T",
            &["Item".to_string(), "Qty".to_string(), "Code".to_string()],
            &[
                vec![json!("Bolt"), json!(12), json!("0012")],
                vec![json!("Nut"), json!(null), json!("0013")],
            ],
            None,
        );
        assert!(html.contains(r#"<th scope="col" class="num">Qty</th>"#));
        assert!(html.contains(r#"<td class="num">12</td>"#));
        // A column of digit *strings* is text, and so is the row header.
        assert!(html.contains(r#"<th scope="col">Code</th>"#));
        assert!(html.contains("<td>0012</td>"));
        assert!(html.contains(r#"<th scope="row">Bolt</th>"#));
    }

    #[test]
    fn carries_no_styling_of_its_own() {
        let html = render("T", &["A".to_string()], &[vec![json!(1)]], None);
        assert!(!html.contains("<style"));
        assert!(!html.contains("style="));
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
        assert!(html.contains("&lt;b&gt;T&lt;/b&gt;"));
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
