//! Bar/line charts — a capability the crate never had before this rebuild
//! (the only data-bearing output used to be `generate_accessible_table`).
//! Gutters are computed from measured tick/category label widths rather than
//! fixed, which is exactly the class of bug the diagram layouts otherwise
//! have: a fixed gutter either wastes space or clips a long label.
//!
//! Category labels wrap to up to two lines rather than rotating 45 degrees —
//! rotated text would need its own bounding-box math threaded through
//! `render::svg::Bounds`, and wrapping solves the same "long label" problem
//! (no overlap, no clipping) with primitives this module already has.

use crate::tools::viz::model::{ChartSeries, ChartType};
use crate::tools::viz::render::svg::{SvgCanvas, CANVAS_MARGIN, TITLE_BAND};
use crate::tools::viz::text;

const AXIS_FONT_PX: f32 = 10.5;
const BAR_W: f32 = 28.0;
const BAR_GAP: f32 = 4.0;
const GROUP_GAP: f32 = 28.0;
const PLOT_H: f32 = 260.0;
const LEGEND_ROW_H: f32 = 26.0;
const Y_LABEL_ROW_H: f32 = 18.0;
const AXIS_TITLE_ROW_H: f32 = 22.0;
const LABEL_LINE_H: f32 = 13.0;

fn nice_ticks(min_v: f64, max_v: f64, target: usize) -> Vec<f64> {
    if (max_v - min_v).abs() < 1e-9 {
        return vec![min_v - 1.0, min_v, min_v + 1.0];
    }
    let range = max_v - min_v;
    let raw_step = range / target.max(1) as f64;
    let magnitude = 10f64.powf(raw_step.log10().floor());
    let residual = raw_step / magnitude;
    let step = if residual > 5.0 {
        10.0 * magnitude
    } else if residual > 2.0 {
        5.0 * magnitude
    } else if residual > 1.0 {
        2.0 * magnitude
    } else {
        magnitude
    };
    let start = (min_v / step).floor() * step;
    let end = (max_v / step).ceil() * step;
    let mut ticks = Vec::new();
    let mut v = start;
    while v <= end + step * 0.5 && ticks.len() < 20 {
        ticks.push(v);
        v += step;
    }
    ticks
}

fn format_number(v: f64) -> String {
    if v.fract().abs() < 1e-9 {
        format!("{v:.0}")
    } else {
        let s = format!("{v:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn value_bounds(series: &[ChartSeries]) -> (f64, f64) {
    let mut lo = 0.0_f64;
    let mut hi = 0.0_f64;
    for s in series {
        for &v in &s.values {
            lo = lo.min(v);
            hi = hi.max(v);
        }
    }
    if (hi - lo).abs() < 1e-9 {
        hi = lo + 1.0;
    }
    (lo, hi)
}

fn bar_class(idx: usize) -> String {
    format!("bar-{}", idx % 4)
}

fn line_class(idx: usize) -> String {
    format!("series-line dash-{}", idx % 4)
}

pub fn render(chart_type: ChartType, categories: &[String], series: &[ChartSeries], x_label: Option<&str>, y_label: Option<&str>) -> (String, f32, f32) {
    match chart_type {
        ChartType::HorizontalBar => render_horizontal_bar(categories, series, x_label, y_label),
        _ => render_vertical(chart_type, categories, series, x_label, y_label),
    }
}

fn render_vertical(chart_type: ChartType, categories: &[String], series: &[ChartSeries], x_label: Option<&str>, y_label: Option<&str>) -> (String, f32, f32) {
    let (min_v, max_v) = value_bounds(series);
    let ticks = nice_ticks(min_v, max_v, 5);
    let (tick_lo, tick_hi) = (ticks.first().copied().unwrap_or(min_v), ticks.last().copied().unwrap_or(max_v));

    let tick_labels: Vec<String> = ticks.iter().map(|&t| format_number(t)).collect();
    let widest_tick = tick_labels.iter().map(|l| text::measure_px(l, AXIS_FONT_PX)).fold(0.0_f32, f32::max);
    let left_gutter = widest_tick + 16.0;

    let bars_per_slot = if chart_type == ChartType::Line { 0 } else { series.len().max(1) };
    let slot_content_w = if bars_per_slot > 0 {
        bars_per_slot as f32 * BAR_W + (bars_per_slot as f32 - 1.0).max(0.0) * BAR_GAP
    } else {
        0.0
    };
    let slot_w = (slot_content_w + GROUP_GAP).max(60.0);

    let legend_h = if series.len() > 1 { LEGEND_ROW_H } else { 0.0 };
    let y_label_h = if y_label.is_some() { Y_LABEL_ROW_H } else { 0.0 };
    let plot_top = TITLE_BAND + legend_h + y_label_h;
    let plot_bottom = plot_top + PLOT_H;
    let plot_left = left_gutter;
    let plot_w = slot_w * categories.len() as f32;

    let value_to_y = |v: f64| -> f32 { plot_bottom - ((v - tick_lo) / (tick_hi - tick_lo)) as f32 * PLOT_H };

    let mut canvas = SvgCanvas::new();

    if let Some(y_label) = y_label {
        canvas.text_line(plot_left, TITLE_BAND + 12.0, "axis-title", y_label);
    }
    if series.len() > 1 {
        let mut lx = plot_left;
        for (j, s) in series.iter().enumerate() {
            canvas.rect(lx, plot_top - legend_h + 6.0, 12.0, 12.0, &format!("legend-swatch {}", bar_class(j)));
            canvas.text_line(lx + 16.0, plot_top - legend_h + 16.0, "legend-text", &s.name);
            lx += 16.0 + text::measure_px(&s.name, AXIS_FONT_PX) + 20.0;
        }
    }

    for &t in &ticks {
        let y = value_to_y(t);
        canvas.line(plot_left, y, plot_left + plot_w, y, "grid-line");
        canvas.text_line(plot_left - 10.0, y + 3.5, "axis-label-end", &format_number(t));
    }
    canvas.line(plot_left, plot_top, plot_left, plot_bottom, "axis-line");
    let zero_y = value_to_y(0.0_f64.clamp(tick_lo, tick_hi));
    canvas.line(plot_left, zero_y, plot_left + plot_w, zero_y, "axis-line");

    for (i, category) in categories.iter().enumerate() {
        let slot_x = plot_left + i as f32 * slot_w;
        let slot_cx = slot_x + slot_w / 2.0;

        if chart_type != ChartType::Line {
            let group_w = slot_content_w;
            let mut bx = slot_cx - group_w / 2.0;
            for (j, s) in series.iter().enumerate() {
                let v = s.values[i];
                let y_top = value_to_y(v).min(zero_y);
                let y_bottom = value_to_y(v).max(zero_y);
                canvas.rect(bx, y_top, BAR_W, (y_bottom - y_top).max(1.0), &bar_class(j));
                canvas.text_line(bx + BAR_W / 2.0, y_top - 6.0, "value-label", &format_number(v));
                bx += BAR_W + BAR_GAP;
            }
        }

        let lines = text::wrap(category, slot_w - 6.0, AXIS_FONT_PX, 2);
        canvas.text_lines(slot_cx, plot_bottom + 14.0, "axis-label", &lines, LABEL_LINE_H);
    }

    if chart_type == ChartType::Line {
        for (j, s) in series.iter().enumerate() {
            let points: Vec<(f32, f32)> =
                (0..categories.len()).map(|i| (plot_left + (i as f32 + 0.5) * slot_w, value_to_y(s.values[i]))).collect();
            if points.len() >= 2 {
                let mut d = format!("M {:.1},{:.1}", points[0].0, points[0].1);
                for p in &points[1..] {
                    d.push_str(&format!(" L {:.1},{:.1}", p.0, p.1));
                }
                let min_x = points.iter().map(|p| p.0).fold(f32::MAX, f32::min);
                let max_x = points.iter().map(|p| p.0).fold(f32::MIN, f32::max);
                let min_y = points.iter().map(|p| p.1).fold(f32::MAX, f32::min);
                let max_y = points.iter().map(|p| p.1).fold(f32::MIN, f32::max);
                canvas.path(&d, &line_class(j), (min_x, min_y, max_x - min_x, max_y - min_y));
            }
            for &(x, y) in &points {
                canvas.circle(x, y, 5.0, "data-point");
            }
        }
    }

    if let Some(x_label) = x_label {
        canvas.text_line(plot_left + plot_w / 2.0, plot_bottom + 2.0 * LABEL_LINE_H + AXIS_TITLE_ROW_H, "axis-title-center", x_label);
    }

    canvas.reserve(0.0, 0.0, plot_left + plot_w, plot_bottom + 2.0 * LABEL_LINE_H + AXIS_TITLE_ROW_H);

    let (body, bounds) = canvas.into_parts();
    (body, bounds.width() + CANVAS_MARGIN, bounds.height() + CANVAS_MARGIN)
}

fn render_horizontal_bar(categories: &[String], series: &[ChartSeries], x_label: Option<&str>, y_label: Option<&str>) -> (String, f32, f32) {
    let (min_v, max_v) = value_bounds(series);
    let ticks = nice_ticks(min_v, max_v, 5);
    let (tick_lo, tick_hi) = (ticks.first().copied().unwrap_or(min_v), ticks.last().copied().unwrap_or(max_v));

    let left_gutter = categories.iter().map(|c| text::measure_px(c, AXIS_FONT_PX)).fold(0.0_f32, f32::max) + 16.0;
    let legend_h = if series.len() > 1 { LEGEND_ROW_H } else { 0.0 };
    let plot_top = TITLE_BAND + legend_h;
    let plot_w = 640.0_f32;
    let bars_per_slot = series.len().max(1);
    let row_content_h = bars_per_slot as f32 * BAR_W + (bars_per_slot as f32 - 1.0).max(0.0) * BAR_GAP;
    let row_h = row_content_h + GROUP_GAP;

    let value_to_x = |v: f64| -> f32 { left_gutter + ((v - tick_lo) / (tick_hi - tick_lo)) as f32 * plot_w };

    let mut canvas = SvgCanvas::new();

    if series.len() > 1 {
        let mut lx = left_gutter;
        for (j, s) in series.iter().enumerate() {
            canvas.rect(lx, plot_top - legend_h + 6.0, 12.0, 12.0, &format!("legend-swatch {}", bar_class(j)));
            canvas.text_line(lx + 16.0, plot_top - legend_h + 16.0, "legend-text", &s.name);
            lx += 16.0 + text::measure_px(&s.name, AXIS_FONT_PX) + 20.0;
        }
    }

    let plot_bottom = plot_top + row_h * categories.len() as f32;
    for &t in &ticks {
        let x = value_to_x(t);
        canvas.line(x, plot_top, x, plot_bottom, "grid-line");
        canvas.text_line(x, plot_bottom + 16.0, "axis-label", &format_number(t));
    }
    let zero_x = value_to_x(0.0_f64.clamp(tick_lo, tick_hi));
    canvas.line(zero_x, plot_top, zero_x, plot_bottom, "axis-line");
    canvas.line(left_gutter, plot_top, left_gutter, plot_bottom, "axis-line");

    for (i, category) in categories.iter().enumerate() {
        let row_top = plot_top + i as f32 * row_h;
        let row_cy = row_top + row_content_h / 2.0;
        canvas.text_line(left_gutter - 10.0, row_cy + 3.5, "axis-label-end", category);

        let mut by = row_top;
        for (j, s) in series.iter().enumerate() {
            let v = s.values[i];
            let x_left = value_to_x(v).min(zero_x);
            let x_right = value_to_x(v).max(zero_x);
            canvas.rect(x_left, by, (x_right - x_left).max(1.0), BAR_W, &bar_class(j));
            canvas.text_line(x_right + 6.0, by + BAR_W / 2.0 + 3.5, "value-label-start", &format_number(v));
            by += BAR_W + BAR_GAP;
        }
    }

    if let Some(x_label) = x_label {
        canvas.text_line(left_gutter + plot_w / 2.0, plot_bottom + 2.0 * LABEL_LINE_H + AXIS_TITLE_ROW_H, "axis-title-center", x_label);
    }
    if let Some(y_label) = y_label {
        canvas.text_line(0.0, plot_top - 10.0, "axis-title", y_label);
    }

    canvas.reserve(0.0, 0.0, left_gutter + plot_w + 80.0, plot_bottom + 2.0 * LABEL_LINE_H + AXIS_TITLE_ROW_H);

    let (body, bounds) = canvas.into_parts();
    (body, bounds.width() + CANVAS_MARGIN, bounds.height() + CANVAS_MARGIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(name: &str, values: &[f64]) -> ChartSeries {
        ChartSeries { name: name.to_string(), values: values.to_vec() }
    }

    #[test]
    fn nice_ticks_span_the_data_range() {
        let ticks = nice_ticks(0.0, 87.0, 5);
        assert!(*ticks.first().unwrap() <= 0.0);
        assert!(*ticks.last().unwrap() >= 87.0);
        assert!(ticks.len() >= 2);
    }

    #[test]
    fn nice_ticks_handles_degenerate_flat_data() {
        let ticks = nice_ticks(5.0, 5.0, 5);
        assert!(ticks.len() >= 2);
    }

    #[test]
    fn bar_chart_renders_caller_categories_and_values() {
        let categories = vec!["Q1".to_string(), "Q2".to_string(), "Q3".to_string(), "Q4".to_string()];
        let series = vec![series("Revenue", &[12.4, 15.1, 22.8, 24.0])];
        let (body, _, _) = render(ChartType::Bar, &categories, &series, None, Some("USD millions"));
        assert!(body.contains("Q1"));
        assert!(body.contains("24"));
    }

    #[test]
    fn grouped_bar_draws_one_bar_per_series_per_category() {
        let categories = vec!["Q1".to_string(), "Q2".to_string()];
        let series = vec![series("2024", &[10.0, 20.0]), series("2025", &[15.0, 25.0])];
        let (body, _, _) = render(ChartType::GroupedBar, &categories, &series, None, None);
        assert!(body.contains("bar-0"));
        assert!(body.contains("bar-1"));
        assert!(body.contains("2024"));
        assert!(body.contains("2025"));
    }

    #[test]
    fn line_chart_draws_a_path_and_points() {
        let categories = vec!["Jan".to_string(), "Feb".to_string(), "Mar".to_string()];
        let series = vec![series("Users", &[100.0, 150.0, 130.0])];
        let (body, _, _) = render(ChartType::Line, &categories, &series, None, None);
        assert!(body.contains("series-line"));
        assert!(body.contains("data-point"));
    }

    #[test]
    fn horizontal_bar_handles_long_category_names() {
        let categories = vec!["A very long category name indeed".to_string(), "Short".to_string()];
        let series = vec![series("Value", &[42.0, 7.0])];
        let (body, w, _) = render(ChartType::HorizontalBar, &categories, &series, None, None);
        assert!(body.contains("A very long category name indeed"));
        assert!(w > 0.0);
    }

    #[test]
    fn every_bar_stays_within_canvas_bounds() {
        let categories: Vec<String> = (0..10).map(|i| format!("Category {i} with a longer label")).collect();
        let values: Vec<f64> = (0..10).map(|i| i as f64 * 3.5).collect();
        let series = vec![series("S", &values)];
        let (body, w, h) = render(ChartType::Bar, &categories, &series, Some("Category"), Some("Value"));
        for (x, y, rw, rh) in extract_rects(&body) {
            assert!(x + rw <= w + 0.5, "rect exceeds canvas width: x={x} rw={rw} w={w}");
            assert!(y + rh <= h + 0.5, "rect exceeds canvas height: y={y} rh={rh} h={h}");
        }
    }

    fn extract_rects(svg: &str) -> Vec<(f32, f32, f32, f32)> {
        let mut out = Vec::new();
        for cap_start in svg.match_indices("<rect ") {
            let tag_end = svg[cap_start.0..].find('/').map(|i| cap_start.0 + i).unwrap_or(svg.len());
            let tag = &svg[cap_start.0..tag_end];
            let get = |attr: &str| -> f32 {
                tag.split(&format!(r#"{attr}=""#)).nth(1).and_then(|rest| rest.split('"').next()).and_then(|v| v.parse().ok()).unwrap_or(0.0)
            };
            out.push((get("x"), get("y"), get("width"), get("height")));
        }
        out
    }
}
