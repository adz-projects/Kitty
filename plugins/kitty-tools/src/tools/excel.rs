//! Excel read tools — Rust port of `kitty_docs_web.py`'s `lean_excel_inspect`
//! and `lean_excel_read_rows`.
//!
//! The write tool (`lean_excel_write_rows`) is deliberately NOT ported: per
//! the plan, spreadsheet *writing* goes through the existing `lean_file_*`
//! CSV tools instead of reintroducing a lossy xlsx writer into this small
//! frozen binary. Everything a caller could do with the write tool — create
//! or extend a spreadsheet the model can hand off — it can do by writing a
//! `.csv` and opening it in Excel.
//!
//! Reading is `calamine` (read-only, pure Rust), replacing openpyxl. Tool
//! names, JSON envelope, error codes and the row-cap pagination contract are
//! kept byte-identical to the Python original. One broader-than-openpyxl
//! behavior: calamine also reads `.xls`/`.ods`, which openpyxl couldn't — the
//! envelope is unchanged regardless of format.

use std::path::Path;

use calamine::{open_workbook_auto, Data, Range, Reader};
use serde_json::{json, Value};

use crate::envelope::{error_response, success_response};
use crate::query_filter::filter_by_query;
use crate::paths::{path_within_home, resolve};

/// Same default as the Python plugin's `EXCEL_MAX_ROWS_DEFAULT` — rows beyond
/// this per page are exposed via `next_offset`, never dumped unbounded.
pub const EXCEL_MAX_ROWS_DEFAULT: usize = 500;

/// Home boundary shared by both Excel tools — defense-in-depth before any
/// filesystem access (the daemon is the primary gate).
fn outside_home(resolved: &Path) -> Option<String> {
    if path_within_home(resolved) {
        None
    } else {
        Some(error_response(
            "PATH_OUTSIDE_HOME",
            "Path is outside the HOME directory",
            Some(&resolved.to_string_lossy()),
            Some("Only paths inside your home directory can be accessed."),
        ))
    }
}

fn open(path: &Path) -> Result<calamine::Sheets<std::io::BufReader<std::fs::File>>, String> {
    open_workbook_auto(path).map_err(|e| e.to_string())
}

/// An integer-valued float is emitted as a JSON integer — openpyxl returns
/// Python `int` for an integral cell, so `1` must serialize as `1`, not `1.0`.
fn float_to_value(f: f64) -> Value {
    if f.fract() == 0.0 && f.is_finite() && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
        json!(f as i64)
    } else {
        json!(f)
    }
}

fn float_to_display(f: f64) -> String {
    if f.fract() == 0.0 && f.is_finite() && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
        (f as i64).to_string()
    } else {
        f.to_string()
    }
}

/// Renders a cell as the JSON value openpyxl's raw value would serialize to.
fn data_to_value(d: &Data) -> Value {
    match d {
        Data::Int(i) => json!(i),
        Data::Float(f) => float_to_value(*f),
        Data::String(s) => json!(s),
        Data::Bool(b) => json!(b),
        Data::DateTime(dt) => dt
            .as_datetime()
            .map(|ndt| json!(ndt.format("%Y-%m-%d %H:%M:%S").to_string()))
            .unwrap_or(Value::Null),
        Data::DateTimeIso(s) => json!(s),
        Data::DurationIso(s) => json!(s),
        Data::Error(_) => Value::Null,
        Data::Empty => Value::Null,
    }
}

/// Renders a cell the way Python's `str(cell)` would for a header row — None
/// for an empty cell (so the caller substitutes `col_{n}`), otherwise the
/// display string. Bool matches Python's `True`/`False` capitalization.
fn display_data(d: &Data) -> Option<String> {
    match d {
        Data::Empty => None,
        Data::String(s) => Some(s.clone()),
        Data::Int(i) => Some(i.to_string()),
        Data::Float(f) => Some(float_to_display(*f)),
        Data::Bool(b) => Some(if *b { "True".to_string() } else { "False".to_string() }),
        Data::DateTime(dt) => dt
            .as_datetime()
            .map(|ndt| ndt.format("%Y-%m-%d %H:%M:%S").to_string()),
        Data::DateTimeIso(s) => Some(s.clone()),
        Data::DurationIso(s) => Some(s.clone()),
        Data::Error(_) => Some(String::new()),
    }
}

fn col_letter(n: u32) -> String {
    let mut s = String::new();
    let mut n = n;
    while n > 0 {
        let rem = (n - 1) % 26;
        s.insert(0, (b'A' + rem as u8) as char);
        n = (n - 1) / 26;
    }
    s
}

fn col_letter_to_num(s: &str) -> Option<u32> {
    let mut col: u64 = 0;
    for c in s.chars() {
        let c = c.to_ascii_uppercase();
        if !c.is_ascii_alphabetic() {
            return None;
        }
        col = col * 26 + ((c as u8) - b'A' + 1) as u64;
    }
    if col > u32::MAX as u64 {
        None
    } else {
        Some(col as u32)
    }
}

/// Parses `openpyxl.utils.cell.range_boundaries`-style input ("A1:C3", or a
/// single "B2") into 1-based inclusive `(min_col, min_row, max_col, max_row)`.
fn parse_range_boundaries(s: &str) -> Option<(u32, u32, u32, u32)> {
    let (a, b) = match s.split_once(':') {
        Some((l, r)) => (l.trim(), r.trim()),
        None => (s.trim(), s.trim()),
    };
    if a.is_empty() || b.is_empty() {
        return None;
    }
    let ca = parse_cell(a)?;
    let cb = parse_cell(b)?;
    Some((
        ca.1.min(cb.1),
        ca.0.min(cb.0),
        ca.1.max(cb.1),
        ca.0.max(cb.0),
    ))
}

/// Parses "A1" into `(row, col)` (1-based).
fn parse_cell(s: &str) -> Option<(u32, u32)> {
    let letters: String = s
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    if letters.is_empty() {
        return None;
    }
    let digits: &str = &s[letters.len()..];
    if digits.is_empty() {
        return None;
    }
    let row: u32 = digits.parse().ok()?;
    let col = col_letter_to_num(&letters)?;
    Some((row, col))
}

pub fn excel_inspect(path: &str) -> String {
    let resolved = resolve(path);
    if let Some(err) = outside_home(&resolved) {
        return err;
    }
    if !resolved.exists() {
        return error_response("XLSX_NOT_FOUND", "Spreadsheet does not exist", Some(&resolved.to_string_lossy()), None);
    }

    let mut wb = match open(&resolved) {
        Ok(wb) => wb,
        Err(e) => {
            return error_response("XLSX_CORRUPT", &format!("Cannot open workbook: {e}"), Some(&resolved.to_string_lossy()), None);
        }
    };

    let sheet_names = wb.sheet_names();
    if sheet_names.is_empty() {
        return success_response(json!({
            "sheet_names": [],
            "active_sheet": Value::Null,
            "headers": [],
            "dimensions": Value::Null,
            "max_rows": 0,
            "max_cols": 0,
        }), None, false, None);
    }

    let active = sheet_names[0].clone();
    let headers: Value = match wb.worksheet_range_at(0) {
        Some(Ok(range)) => {
            let first_row = range.rows().next();
            json!(first_row
                .map(|r| {
                    let mut v: Vec<String> = Vec::new();
                    for (i, c) in r.iter().enumerate() {
                        v.push(display_data(c).unwrap_or_else(|| format!("col_{}", i + 1)));
                    }
                    v
                })
                .unwrap_or_default())
        }
        _ => json!([]),
    };

    let (dimensions, max_rows, max_cols) = match wb.worksheet_range_at(0) {
        Some(Ok(range)) => {
            if range.is_empty() {
                ("A1:A1".to_string(), 0, 0)
            } else {
                // calamine coordinates are 0-based (row, col); openpyxl's
                // `ws.dimensions`/`max_row`/`max_column` are 1-based letters.
                let start = range.start().unwrap_or((0, 0));
                let end = range.end().unwrap_or((0, 0));
                let start_cell = format!("{}{}", col_letter(start.1 + 1), start.0 + 1);
                let end_cell = format!("{}{}", col_letter(end.1 + 1), end.0 + 1);
                let dim = format!("{start_cell}:{end_cell}");
                (dim, end.0 as u64 + 1, end.1 as u64 + 1)
            }
        }
        _ => ("A1:A1".to_string(), 0, 0),
    };

    success_response(json!({
        "sheet_names": sheet_names,
        "active_sheet": active,
        "headers": headers,
        "dimensions": dimensions,
        "max_rows": max_rows,
        "max_cols": max_cols,
    }), None, false, None)
}

/// `output_format` is `"json"` (default) or `"csv"`.
pub fn excel_read_rows(
    path: &str,
    sheet: Option<&str>,
    range_box: Option<&str>,
    output_format: &str,
    query: Option<&str>,
    offset: usize,
) -> String {
    let resolved = resolve(path);
    if let Some(err) = outside_home(&resolved) {
        return err;
    }
    if !resolved.exists() {
        return error_response("XLSX_NOT_FOUND", "Spreadsheet does not exist", Some(&resolved.to_string_lossy()), None);
    }

    let mut wb = match open(&resolved) {
        Ok(wb) => wb,
        Err(e) => {
            return error_response("XLSX_CORRUPT", &format!("Cannot open workbook: {e}"), Some(&resolved.to_string_lossy()), None);
        }
    };

    let sheet_names = wb.sheet_names();
    let ws_name = sheet.unwrap_or("").to_string();
    let idx = if ws_name.is_empty() {
        if sheet_names.is_empty() {
            return success_response(json!([]), None, false, None);
        }
        0
    } else {
        match sheet_names.iter().position(|n| n == &ws_name) {
            Some(i) => i,
            None => {
                return error_response("XLSX_BAD_SHEET", &format!("Sheet '{ws_name}' not found"), Some(&resolved.to_string_lossy()), None);
            }
        }
    };

    // The range-box column/row window (1-based inclusive), optional.
    let window: Option<(u32, u32, u32, u32)> = match range_box {
        Some(rb) if !rb.trim().is_empty() => match parse_range_boundaries(rb) {
            Some(w) => Some(w),
            None => {
                return error_response("XLSX_BAD_RANGE", &format!("Invalid range '{rb}'"), Some(&resolved.to_string_lossy()), None);
            }
        },
        _ => None,
    };

    let range: Range<Data> = match wb.worksheet_range_at(idx) {
        Some(Ok(r)) => r,
        _ => {
            return error_response("XLSX_CORRUPT", "Cannot read worksheet cells", Some(&resolved.to_string_lossy()), None);
        }
    };
    // calamine trims to the used box; each `rows()` row is padded to the box
    // width (matching openpyxl's `iter_rows(values_only=True)` reaching
    // `ws.max_column`). Coordinates below are translated from calamine's
    // 0-based (row,col) to 1-based sheet rows/cols via the range start.
    let range_start = range.start().unwrap_or((0, 0));
    let range_end = range.end().unwrap_or(range_start);
    let surviving_rows = surviving_row_count(range_start, range_end, window);

    if surviving_rows == 0 {
        return success_response(json!([]), None, false, None);
    }

    // Query path: keyword-search every surviving data row, but only
    // JSON-build the matched page — a million-row sheet is scanned as
    // strings without ever holding the whole sheet as Value objects.
    if let Some(q) = query.filter(|q| !q.trim().is_empty()) {
        let mut headers: Vec<String> = Vec::new();
        let mut serialized: Vec<String> = Vec::new();
        let mut survivor = 0usize;
        for (row_idx, row) in range.rows().enumerate() {
            if let Some(clipped) = clip_row(row, range_start, row_idx, window) {
                if survivor == 0 {
                    headers = build_headers(&clipped);
                } else {
                    let obj = row_to_obj(&clipped, &headers);
                    serialized.push(serde_json::to_string(&obj).unwrap_or_default());
                }
                survivor += 1;
            }
        }
        let result = filter_by_query(&serialized, Some(q), 50, offset);
        let filtered: Vec<Value> = result
            .items
            .iter()
            .filter_map(|s| serde_json::from_str(s).ok())
            .collect();
        let message = result
            .no_match
            .then(|| format!("No direct matches for query '{q}'. Showing top section."));
        let mut meta = serde_json::Map::new();
        meta.insert("filtered_by_query".into(), json!(q));
        meta.insert("total_matches".into(), json!(result.total_matches));
        meta.insert("offset".into(), json!(offset));
        if let Some(next) = result.next_offset {
            meta.insert("next_offset".into(), json!(next));
        }
        let data = if filtered.is_empty() {
            Value::Array(result.items.into_iter().map(Value::String).collect())
        } else {
            Value::Array(filtered)
        };
        return success_response(data, message.as_deref(), result.truncated, Some(Value::Object(meta)));
    }

    // No-query path: materialize only the header row and the page window,
    // not the whole sheet — reading row 1 of a million-row sheet no longer
    // clones every row before slicing.
    let limit = EXCEL_MAX_ROWS_DEFAULT;
    let mut header_row: Option<Vec<Data>> = None;
    let mut page_raw: Vec<Vec<Data>> = Vec::new();
    let page_start = 1 + offset; // survivor position of the first page row
    let page_end = 1 + offset + limit; // (exclusive)
    let mut survivor = 0usize;
    'rows: for (row_idx, row) in range.rows().enumerate() {
        if let Some(clipped) = clip_row(row, range_start, row_idx, window) {
            if survivor == 0 {
                header_row = Some(clipped);
            } else if survivor >= page_start && survivor < page_end {
                page_raw.push(clipped);
                if page_raw.len() == limit {
                    break 'rows;
                }
            }
            survivor += 1;
        }
    }

    // Headers from the first row; empty header cells become `col_{n}`.
    let headers = build_headers(header_row.as_deref().unwrap_or_default());

    let page: Vec<Value> = page_raw.iter().map(|row| row_to_obj(row, &headers)).collect();
    let total_rows = surviving_rows.saturating_sub(1);
    let has_more = offset + page.len() < total_rows;
    let mut meta = serde_json::Map::new();
    meta.insert("total_rows".into(), json!(total_rows));
    meta.insert("offset".into(), json!(offset));
    if has_more {
        meta.insert("next_offset".into(), json!(offset + page.len()));
    }

    if output_format == "csv" {
        let mut lines: Vec<String> = Vec::new();
        lines.push(csv_line(&headers));
        for r in &page {
            let mut fields: Vec<String> = Vec::new();
            for h in &headers {
                let v = r.get(h).unwrap_or(&Value::Null);
                fields.push(cell_to_csv(v));
            }
            lines.push(csv_line(&fields));
        }
        return success_response(json!(lines.join("\r\n")), None, has_more, Some(Value::Object(meta)));
    }

    success_response(json!(page), None, has_more, Some(Value::Object(meta)))
}

/// Applies the range-box window to one calamine row slice, returning the
/// surviving (column-clipped) cells, or `None` when the row is outside the
/// row window. Identical slicing to the former full-sheet `rows` pass.
fn clip_row(
    row: &[Data],
    range_start: (u32, u32),
    row_idx: usize,
    window: Option<(u32, u32, u32, u32)>,
) -> Option<Vec<Data>> {
    match window {
        None => Some(row.to_vec()),
        Some((min_col, min_row, max_col, max_row)) => {
            let sheet_row = range_start.0 + row_idx as u32 + 1;
            if sheet_row < min_row || sheet_row > max_row {
                return None;
            }
            let mut clipped = Vec::new();
            for (col_idx, cell) in row.iter().enumerate() {
                let sheet_col = range_start.1 + col_idx as u32 + 1;
                if sheet_col >= min_col && sheet_col <= max_col {
                    clipped.push(cell.clone());
                }
            }
            Some(clipped)
        }
    }
}

/// Number of rows that survive the row-window, computed arithmetically from
/// the used-range coverage and the 1-based range box — no iteration, so a
/// page request never has to walk the whole sheet just to report `total_rows`.
fn surviving_row_count(
    range_start: (u32, u32),
    range_end: (u32, u32),
    window: Option<(u32, u32, u32, u32)>,
) -> usize {
    let cover_start = range_start.0 + 1; // 1-based first covered row
    let cover_end = range_end.0 + 1; // 1-based last covered row
    let (min_row, max_row) = match window {
        Some((_, min_row, _, max_row)) => (min_row, max_row),
        None => (1, u32::MAX),
    };
    let lo = min_row.max(cover_start);
    let hi = max_row.min(cover_end);
    if hi >= lo {
        (hi - lo + 1) as usize
    } else {
        0
    }
}

/// Builds header strings from the first (surviving) row; empty cells become
/// `col_{n}` — identical to the former `rows[0]` handling.
fn build_headers(header_row: &[Data]) -> Vec<String> {
    header_row
        .iter()
        .enumerate()
        .map(|(i, c)| display_data(c).unwrap_or_else(|| format!("col_{}", i + 1)))
        .collect()
}

/// Builds a JSON object for one data row keyed by the headers.
fn row_to_obj(row: &[Data], headers: &[String]) -> Value {
    let mut obj = serde_json::Map::new();
    for (i, h) in headers.iter().enumerate() {
        obj.insert(
            h.clone(),
            row.get(i).map(data_to_value).unwrap_or(Value::Null),
        );
    }
    Value::Object(obj)
}

fn cell_to_csv(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        // Match Python's `csv.writer` + `str(bool)` -> "True"/"False", which
        // Excel understands as booleans when importing a CSV.
        Value::Bool(b) => if *b { "True".to_string() } else { "False".to_string() },
        other => other.to_string(),
    }
}

/// Minimal RFC-4180-ish CSV field quoting, close to Python's `csv.writer`.
fn csv_line(fields: &[String]) -> String {
    fields
        .iter()
        .map(|f| {
            if f.contains(',') || f.contains('"') || f.contains('\n') || f.contains('\r') {
                format!("\"{}\"", f.replace('"', "\"\""))
            } else {
                f.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn col_letters_round_trip() {
        assert_eq!(col_letter(1), "A");
        assert_eq!(col_letter(26), "Z");
        assert_eq!(col_letter(27), "AA");
        assert_eq!(col_letter_to_num("A"), Some(1));
        assert_eq!(col_letter_to_num("AA"), Some(27));
        assert_eq!(col_letter_to_num("ZZ"), Some(702));
    }

    #[test]
    fn range_boundaries_parse_boxes() {
        assert_eq!(parse_range_boundaries("A1:C3"), Some((1, 1, 3, 3)));
        assert_eq!(parse_range_boundaries("B2"), Some((2, 2, 2, 2)));
        assert_eq!(parse_range_boundaries("B2:A1"), Some((1, 1, 2, 2)));
        assert!(parse_range_boundaries("garbage").is_none());
    }

    #[test]
    fn data_to_value_maps_cell_types() {
        assert_eq!(data_to_value(&Data::Int(42)), json!(42));
        assert_eq!(data_to_value(&Data::String("x".into())), json!("x"));
        assert_eq!(data_to_value(&Data::Bool(true)), json!(true));
        assert_eq!(data_to_value(&Data::Empty), Value::Null);
    }

    #[test]
    fn outside_home_path_is_rejected() {
        #[cfg(windows)]
        let p = "C:\\Windows\\System32\\drivers\\etc\\hosts";
        #[cfg(not(windows))]
        let p = "/etc/passwd";
        let s = excel_inspect(p);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error_code"], "PATH_OUTSIDE_HOME");

        let s = excel_read_rows(p, None, None, "json", None, 0);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error_code"], "PATH_OUTSIDE_HOME");
    }
}
