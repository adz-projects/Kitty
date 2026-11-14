//! Bar and line charts.
//!
//! The layout is a stack of rows, top to bottom, each of which owns its own
//! band of the canvas: legend, value-axis title, headroom for value labels,
//! the plot, category labels, category-axis title. Nothing is positioned
//! relative to a neighbour's *baseline* — the old layout did that, and it is
//! why labels landed on the axes (a two-line category label was centred on its
//! anchor and climbed back over the x-axis; the y-axis title shared a baseline
//! with the tallest bar's value label). Every label here hangs from the top of
//! its own band and can only grow away from the plot.
//!
//! Text widths come from `text::measure_px`. The host stylesheet sets these
//! charts in a Helvetica/Arial stack, which is the face that table was built
//! from, so a measured width is the rendered width (plus the safety margin).
//!
//! Where a label cannot fit, it is dropped or turned rather than squeezed:
//! value labels are all-or-nothing per chart, and category labels that will
//! not fit their slot in two lines are set at 45 degrees with the space that
//! needs reserved up front.

use crate::tools::viz::layout::MAX_CONTENT_W;
use crate::tools::viz::model::{ChartSeries, ChartType};
use crate::tools::viz::render::svg::{SvgCanvas, CANVAS_MARGIN, TOP_PAD};
use crate::tools::viz::text;

/// Canvas width a chart is laid out to. The chat column is about this wide,
/// and the host never upscales, so type renders at the size set here.
const TARGET_W: f32 = 640.0;
/// Matches the 11.5px tick/category/legend/axis-title rules in the host CSS.
const LABEL_FONT_PX: f32 = 11.5;
/// Matches the 11px `.value-label*` rules.
const VALUE_FONT_PX: f32 = 11.0;
const LABEL_LINE_H: f32 = 14.0;
/// Baseline offset that centres a line of text on a y-coordinate, as a
/// fraction of the font size (cap-height / 2 for a grotesque). Done here in
/// arithmetic rather than with `dominant-baseline`, which WebViews disagree on.
const MID_BASELINE: f32 = 0.36;

const PLOT_H: f32 = 240.0;
const MAX_SLOT_W: f32 = 112.0;
const MAX_BAR_W: f32 = 44.0;
const MIN_BAR_W: f32 = 4.0;
const BAR_GAP: f32 = 2.0;
/// Gap between the tick labels and the plot's left edge.
const TICK_GAP: f32 = 8.0;
const RIGHT_PAD: f32 = 10.0;
const LEGEND_ROW_H: f32 = 20.0;
const AXIS_TITLE_ROW_H: f32 = 20.0;
/// Space kept clear above the tallest bar (and below the deepest negative
/// one) for its value label.
const VALUE_ROOM: f32 = 18.0;
/// Gap between the bottom of the plot and the top of the category labels.
const CATEGORY_GAP: f32 = 7.0;
const ROTATED_LABEL_MAX_W: f32 = 140.0;
/// sin/cos of the 45-degree category-label rotation.
const DIAG: f32 = 0.707;

const HBAR_H: f32 = 16.0;
const HBAR_H_GROUPED: f32 = 12.0;
const HBAR_ROW_GAP: f32 = 12.0;
const HBAR_LABEL_MAX_W: f32 = 180.0;
const HBAR_MIN_PLOT_W: f32 = 240.0;

/// Number of distinct series styles in the host stylesheet (`.bar-0`..`.bar-5`,
/// `.dash-0`..`.dash-5`, `.pt-0`..`.pt-5`); further series cycle.
const SERIES_STYLES: usize = 6;

const MINUS: char = '\u{2212}';

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

/// One number format for a whole chart, so the axis and the value labels
/// agree: millions and billions are abbreviated for every figure or for none
/// (an axis reading `500,000 / 1M / 1.5M` is harder to scan than `0.5M / 1M /
/// 1.5M`), and long integers get thousands separators.
#[derive(Clone, Copy)]
struct NumFmt {
    div: f64,
    suffix: &'static str,
}

impl NumFmt {
    fn for_range(max_abs: f64) -> Self {
        let (div, suffix) = if max_abs >= 1e9 {
            (1e9, "B")
        } else if max_abs >= 1e6 {
            (1e6, "M")
        } else {
            (1.0, "")
        };
        Self { div, suffix }
    }

    fn fmt(&self, v: f64, decimals: usize) -> String {
        let scaled = v / self.div;
        if scaled == 0.0 {
            // Zero carries no unit: `0`, not `0M`.
            return "0".to_string();
        }
        let fixed = format!("{:.*}", decimals, scaled.abs());
        let fixed = if fixed.contains('.') {
            fixed.trim_end_matches('0').trim_end_matches('.')
        } else {
            fixed.as_str()
        };
        let (int_part, frac_part) = match fixed.split_once('.') {
            Some((i, f)) => (i, Some(f)),
            None => (fixed, None),
        };
        let mut out = String::new();
        // A true minus sign, not a hyphen; and never "-0".
        let is_zero = frac_part.is_none() && int_part.chars().all(|c| c == '0');
        if scaled < 0.0 && !is_zero {
            out.push(MINUS);
        }
        let digits: Vec<char> = int_part.chars().collect();
        for (i, c) in digits.iter().enumerate() {
            if i > 0 && (digits.len() - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push(*c);
        }
        if let Some(f) = frac_part {
            out.push('.');
            out.push_str(f);
        }
        out.push_str(self.suffix);
        out
    }

    fn tick(&self, v: f64) -> String {
        self.fmt(v, 3)
    }

    fn value(&self, v: f64) -> String {
        self.fmt(v, 2)
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
    format!("bar bar-{}", idx % SERIES_STYLES)
}

fn line_class(idx: usize) -> String {
    format!("series-line dash-{}", idx % SERIES_STYLES)
}

fn point_class(idx: usize) -> String {
    format!("data-point pt-{}", idx % SERIES_STYLES)
}

fn widest(labels: &[String], font_px: f32) -> f32 {
    labels
        .iter()
        .map(|l| text::measure_px(l, font_px))
        .fold(0.0_f32, f32::max)
}

/// Point markers differ by shape as well as by fill, so line series stay
/// distinguishable in a photocopy.
fn draw_point(canvas: &mut SvgCanvas, x: f32, y: f32, idx: usize) {
    let class = point_class(idx);
    match idx % 4 {
        0 => canvas.circle(x, y, 3.5, &class),
        1 => canvas.rect(x - 3.2, y - 3.2, 6.4, 6.4, &class),
        2 => canvas.polygon(
            &[(x, y - 4.4), (x + 4.4, y), (x, y + 4.4), (x - 4.4, y)],
            &class,
        ),
        _ => canvas.polygon(
            &[(x, y - 4.4), (x + 4.2, y + 3.2), (x - 4.2, y + 3.2)],
            &class,
        ),
    }
}

/// Draws the series legend flush left from `x0`, wrapping to further rows
/// rather than running past `max_x`. Returns the y just below the last row.
fn draw_legend(
    canvas: &mut SvgCanvas,
    series: &[ChartSeries],
    is_line: bool,
    x0: f32,
    max_x: f32,
    top: f32,
) -> f32 {
    if series.len() < 2 {
        return top;
    }
    let key_w = if is_line { 22.0 } else { 10.0 };
    let mut x = x0;
    let mut row_top = top;
    for (j, s) in series.iter().enumerate() {
        let item_w = key_w + 6.0 + text::measure_px(&s.name, LABEL_FONT_PX);
        if x > x0 && x + item_w > max_x {
            x = x0;
            row_top += LEGEND_ROW_H;
        }
        let mid = row_top + 6.0;
        if is_line {
            canvas.line(x, mid, x + key_w, mid, &line_class(j));
            draw_point(canvas, x + key_w / 2.0, mid, j);
        } else {
            canvas.rect(x, row_top + 1.0, 10.0, 10.0, &bar_class(j));
        }
        canvas.text_line(
            x + key_w + 6.0,
            mid + LABEL_FONT_PX * MID_BASELINE,
            "legend-text",
            &s.name,
        );
        x += item_w + 18.0;
    }
    row_top + LEGEND_ROW_H
}

/// True when `wrap` had to cut `original` short (it ends the last line with
/// an ellipsis when content is left over).
fn was_truncated(lines: &[String], original: &str) -> bool {
    let kept: usize = lines
        .iter()
        .map(|l| l.chars().filter(|c| !c.is_whitespace()).count())
        .sum();
    kept != original.chars().filter(|c| !c.is_whitespace()).count()
}

pub fn render(
    chart_type: ChartType,
    categories: &[String],
    series: &[ChartSeries],
    x_label: Option<&str>,
    y_label: Option<&str>,
) -> (String, f32, f32) {
    match chart_type {
        ChartType::HorizontalBar => render_horizontal_bar(categories, series, x_label, y_label),
        _ => render_vertical(chart_type, categories, series, x_label, y_label),
    }
}

fn render_vertical(
    chart_type: ChartType,
    categories: &[String],
    series: &[ChartSeries],
    x_label: Option<&str>,
    y_label: Option<&str>,
) -> (String, f32, f32) {
    let is_line = chart_type == ChartType::Line;
    let n = categories.len().max(1);
    let k = series.len().max(1);

    let (min_v, max_v) = value_bounds(series);
    let ticks = nice_ticks(min_v, max_v, 5);
    let (tick_lo, tick_hi) = (
        ticks.first().copied().unwrap_or(min_v),
        ticks.last().copied().unwrap_or(max_v),
    );
    let fmt = NumFmt::for_range(tick_lo.abs().max(tick_hi.abs()));
    let tick_labels: Vec<String> = ticks.iter().map(|&t| fmt.tick(t)).collect();
    let has_negative = min_v < 0.0;

    // --- Horizontal extent -------------------------------------------------
    let min_slot_w = if is_line {
        24.0
    } else {
        k as f32 * MIN_BAR_W + (k as f32 - 1.0) * BAR_GAP + 8.0
    };
    let slot_for = |gutter: f32| -> f32 {
        let plot_w = (TARGET_W - gutter - RIGHT_PAD)
            .max(min_slot_w * n as f32)
            .min(MAX_CONTENT_W - gutter - RIGHT_PAD);
        (plot_w / n as f32).min(MAX_SLOT_W)
    };
    let mut left_gutter = widest(&tick_labels, LABEL_FONT_PX) + TICK_GAP + 2.0;
    let mut slot_w = slot_for(left_gutter);

    // Category labels: upright in at most two lines if every one fits its
    // slot, otherwise all of them turned 45 degrees.
    let upright: Vec<Vec<String>> = categories
        .iter()
        .map(|c| text::wrap(c, slot_w - 8.0, LABEL_FONT_PX, 2))
        .collect();
    let rotate = categories
        .iter()
        .zip(&upright)
        .any(|(c, lines)| was_truncated(lines, c));
    let rotated: Vec<String> = if rotate {
        categories
            .iter()
            .map(|c| {
                text::wrap(c, ROTATED_LABEL_MAX_W, LABEL_FONT_PX, 1)
                    .into_iter()
                    .next()
                    .unwrap_or_default()
            })
            .collect()
    } else {
        Vec::new()
    };
    if rotate {
        // A turned label runs down and to the LEFT of its slot centre, so the
        // first few can reach past the plot's left edge: widen the gutter
        // until none does.
        for _ in 0..3 {
            let need = rotated
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    text::measure_px(l, LABEL_FONT_PX) * DIAG - (i as f32 + 0.5) * slot_w + 6.0
                })
                .fold(0.0_f32, f32::max);
            if need <= left_gutter {
                break;
            }
            left_gutter = need;
            slot_w = slot_for(left_gutter);
        }
    }
    // Turned labels need about one line-height of pitch; denser than that,
    // label every second (third, ...) category.
    let label_every = if rotate {
        ((LABEL_FONT_PX * 1.5) / slot_w).ceil().max(1.0) as usize
    } else {
        1
    };

    let plot_left = left_gutter;
    let plot_w = slot_w * n as f32;
    let plot_right = plot_left + plot_w;
    let total_w = plot_right + RIGHT_PAD;

    let bar_w =
        ((slot_w * 0.7 - (k as f32 - 1.0) * BAR_GAP) / k as f32).clamp(MIN_BAR_W, MAX_BAR_W);
    let group_w = k as f32 * bar_w + (k as f32 - 1.0) * BAR_GAP;

    // Value labels are all-or-nothing: print them only if every one fits the
    // room its own mark has, so none can run into a neighbour. Otherwise the
    // grid carries the values.
    let value_room_w = if is_line || k == 1 {
        slot_w - 4.0
    } else {
        bar_w + BAR_GAP - 1.0
    };
    let show_values = (!is_line || k == 1)
        && series.iter().all(|s| {
            s.values
                .iter()
                .all(|&v| text::measure_px(&fmt.value(v), VALUE_FONT_PX) <= value_room_w)
        });

    // --- Vertical bands ----------------------------------------------------
    let mut canvas = SvgCanvas::new();
    let mut y = TOP_PAD;
    y = draw_legend(&mut canvas, series, is_line, 0.0, total_w, y);
    if let Some(label) = y_label.filter(|l| !l.trim().is_empty()) {
        canvas.text_line(0.0, y + 12.0, "axis-title", label);
        y += AXIS_TITLE_ROW_H;
    }
    y += if show_values { VALUE_ROOM } else { 8.0 };
    let plot_top = y;
    let plot_bottom = plot_top + PLOT_H;
    let value_to_y =
        |v: f64| -> f32 { plot_bottom - ((v - tick_lo) / (tick_hi - tick_lo)) as f32 * PLOT_H };
    let zero_y = value_to_y(0.0_f64.clamp(tick_lo, tick_hi));

    for (&t, label) in ticks.iter().zip(&tick_labels) {
        let ty = value_to_y(t);
        canvas.line(plot_left, ty, plot_right, ty, "grid-line");
        canvas.text_line(
            plot_left - TICK_GAP,
            ty + LABEL_FONT_PX * MID_BASELINE,
            "tick-label",
            label,
        );
    }

    if !is_line {
        for i in 0..categories.len() {
            let slot_cx = plot_left + (i as f32 + 0.5) * slot_w;
            let mut bx = slot_cx - group_w / 2.0;
            for (j, s) in series.iter().enumerate() {
                let v = s.values[i];
                let vy = value_to_y(v);
                let (top, bottom) = (vy.min(zero_y), vy.max(zero_y));
                canvas.rect(bx, top, bar_w, (bottom - top).max(1.0), &bar_class(j));
                if show_values {
                    let ly = if v < 0.0 {
                        bottom + VALUE_FONT_PX + 2.0
                    } else {
                        top - 5.0
                    };
                    canvas.text_line(bx + bar_w / 2.0, ly, "value-label", &fmt.value(v));
                }
                bx += bar_w + BAR_GAP;
            }
        }
    }
    // The baseline rule goes over the bars' feet, under the line series.
    canvas.line(plot_left, zero_y, plot_right, zero_y, "axis-line");

    if is_line {
        for (j, s) in series.iter().enumerate() {
            let points: Vec<(f32, f32)> = (0..categories.len())
                .map(|i| {
                    (
                        plot_left + (i as f32 + 0.5) * slot_w,
                        value_to_y(s.values[i]),
                    )
                })
                .collect();
            if points.len() >= 2 {
                let mut d = format!("M {:.1},{:.1}", points[0].0, points[0].1);
                for p in &points[1..] {
                    d.push_str(&format!(" L {:.1},{:.1}", p.0, p.1));
                }
                canvas.path(&d, &line_class(j), (plot_left, plot_top, plot_w, PLOT_H));
            }
            for (i, &(px, py)) in points.iter().enumerate() {
                draw_point(&mut canvas, px, py, j);
                if show_values {
                    // Above the point, unless the point is a trough: there the
                    // line leaves upward on both sides and would cross a label
                    // set above it.
                    let v = s.values[i];
                    let lower_than = |o: Option<&f64>| o.is_none_or(|&ov| v < ov);
                    let trough = points.len() > 1
                        && lower_than(i.checked_sub(1).and_then(|p| s.values.get(p)))
                        && lower_than(s.values.get(i + 1));
                    let ly = if trough && py + 9.0 + VALUE_FONT_PX < plot_bottom {
                        py + 9.0 + VALUE_FONT_PX * 0.72
                    } else {
                        py - 8.0
                    };
                    canvas.text_line(px, ly, "value-label", &fmt.value(v));
                }
            }
        }
    }

    // Category labels hang below the plot — below the negative bars' value
    // labels too, when there are any.
    let label_top = plot_bottom
        + CATEGORY_GAP
        + if has_negative && show_values && !is_line {
            VALUE_ROOM - 4.0
        } else {
            0.0
        };
    let labels_bottom = if rotate {
        let mut deepest = 0.0_f32;
        for (i, label) in rotated.iter().enumerate() {
            if i % label_every != 0 {
                continue;
            }
            let slot_cx = plot_left + (i as f32 + 0.5) * slot_w;
            canvas.text_rotated(
                slot_cx + 3.0,
                label_top + LABEL_FONT_PX * 0.8,
                "cat-label-rot",
                label,
                -45.0,
            );
            deepest = deepest.max(text::measure_px(label, LABEL_FONT_PX) * DIAG);
        }
        label_top + LABEL_FONT_PX + deepest
    } else {
        let mut most_lines = 1usize;
        for (i, lines) in upright.iter().enumerate() {
            let slot_cx = plot_left + (i as f32 + 0.5) * slot_w;
            canvas.text_lines_top(
                slot_cx,
                label_top + LABEL_FONT_PX * 0.8,
                "cat-label",
                lines,
                LABEL_LINE_H,
            );
            most_lines = most_lines.max(lines.len());
        }
        label_top + most_lines as f32 * LABEL_LINE_H
    };

    let mut bottom = labels_bottom;
    if let Some(label) = x_label.filter(|l| !l.trim().is_empty()) {
        canvas.text_line(plot_left, bottom + 16.0, "axis-title", label);
        bottom += AXIS_TITLE_ROW_H;
    }

    let (body, bounds) = canvas.into_parts();
    (
        body,
        total_w.max(bounds.width()) + CANVAS_MARGIN,
        bottom + CANVAS_MARGIN,
    )
}

fn render_horizontal_bar(
    categories: &[String],
    series: &[ChartSeries],
    x_label: Option<&str>,
    y_label: Option<&str>,
) -> (String, f32, f32) {
    let k = series.len().max(1);
    let (min_v, max_v) = value_bounds(series);
    let ticks = nice_ticks(min_v, max_v, 5);
    let (tick_lo, tick_hi) = (
        ticks.first().copied().unwrap_or(min_v),
        ticks.last().copied().unwrap_or(max_v),
    );
    let fmt = NumFmt::for_range(tick_lo.abs().max(tick_hi.abs()));
    let tick_labels: Vec<String> = ticks.iter().map(|&t| fmt.tick(t)).collect();
    let has_negative = min_v < 0.0;

    // Category names sit flush left in their own column, wrapped to two lines
    // if long; the column is exactly as wide as its widest line, so a name can
    // never reach the bars.
    let cat_lines: Vec<Vec<String>> = categories
        .iter()
        .map(|c| text::wrap(c, HBAR_LABEL_MAX_W, LABEL_FONT_PX, 2))
        .collect();
    let label_col_w = cat_lines
        .iter()
        .map(|lines| widest(lines, LABEL_FONT_PX))
        .fold(0.0_f32, f32::max);

    // Every bar is labelled with its value just past its end; the room for the
    // widest of those is reserved on the right (and on the left too when some
    // bars run negative), so a label can never be clipped or land on a name.
    let value_labels: Vec<Vec<String>> = series
        .iter()
        .map(|s| s.values.iter().map(|&v| fmt.value(v)).collect())
        .collect();
    let value_w = value_labels
        .iter()
        .map(|l| widest(l, VALUE_FONT_PX))
        .fold(0.0_f32, f32::max);
    let tick_half = |label: Option<&String>| -> f32 {
        label
            .map(|l| text::measure_px(l, LABEL_FONT_PX) / 2.0)
            .unwrap_or(0.0)
    };
    let right_pad = (value_w + 8.0).max(tick_half(tick_labels.last()) + 2.0);
    let plot_left = (label_col_w + 12.0 + if has_negative { value_w + 8.0 } else { 0.0 })
        .max(tick_half(tick_labels.first()));
    let plot_w = (TARGET_W - plot_left - right_pad).max(HBAR_MIN_PLOT_W);
    let plot_right = plot_left + plot_w;
    let total_w = plot_right + right_pad;

    let bar_h = if k == 1 { HBAR_H } else { HBAR_H_GROUPED };
    let group_h = k as f32 * bar_h + (k as f32 - 1.0) * BAR_GAP;
    let row_heights: Vec<f32> = cat_lines
        .iter()
        .map(|lines| group_h.max(lines.len() as f32 * LABEL_LINE_H) + HBAR_ROW_GAP)
        .collect();

    let value_to_x =
        |v: f64| -> f32 { plot_left + ((v - tick_lo) / (tick_hi - tick_lo)) as f32 * plot_w };
    let zero_x = value_to_x(0.0_f64.clamp(tick_lo, tick_hi));

    let mut canvas = SvgCanvas::new();
    let mut y = TOP_PAD;
    y = draw_legend(&mut canvas, series, false, 0.0, total_w, y);
    if let Some(label) = y_label.filter(|l| !l.trim().is_empty()) {
        canvas.text_line(0.0, y + 12.0, "axis-title", label);
        y += AXIS_TITLE_ROW_H;
    }
    y += 4.0;
    let plot_top = y;
    let plot_bottom = plot_top + row_heights.iter().sum::<f32>();

    for &t in &ticks {
        let tx = value_to_x(t);
        canvas.line(tx, plot_top, tx, plot_bottom, "grid-line");
    }

    let mut row_top = plot_top;
    for (i, lines) in cat_lines.iter().enumerate() {
        let row_h = row_heights[i];
        let cy = row_top + row_h / 2.0;
        let first_baseline =
            cy - (lines.len() as f32 - 1.0) * LABEL_LINE_H / 2.0 + LABEL_FONT_PX * MID_BASELINE;
        canvas.text_lines_top(0.0, first_baseline, "cat-label-start", lines, LABEL_LINE_H);

        let mut by = cy - group_h / 2.0;
        for (j, s) in series.iter().enumerate() {
            let v = s.values[i];
            let vx = value_to_x(v);
            let (left, right) = (vx.min(zero_x), vx.max(zero_x));
            canvas.rect(left, by, (right - left).max(1.0), bar_h, &bar_class(j));
            let baseline = by + bar_h / 2.0 + VALUE_FONT_PX * MID_BASELINE;
            if v < 0.0 {
                canvas.text_line(left - 5.0, baseline, "value-label-end", &value_labels[j][i]);
            } else {
                canvas.text_line(
                    right + 5.0,
                    baseline,
                    "value-label-start",
                    &value_labels[j][i],
                );
            }
            by += bar_h + BAR_GAP;
        }
        row_top += row_h;
    }
    canvas.line(zero_x, plot_top, zero_x, plot_bottom, "axis-line");

    let tick_baseline = plot_bottom + CATEGORY_GAP + LABEL_FONT_PX * 0.8;
    for (&t, label) in ticks.iter().zip(&tick_labels) {
        canvas.text_line(value_to_x(t), tick_baseline, "tick-label-mid", label);
    }
    let mut bottom = plot_bottom + CATEGORY_GAP + LABEL_LINE_H;
    if let Some(label) = x_label.filter(|l| !l.trim().is_empty()) {
        canvas.text_line(plot_left, bottom + 16.0, "axis-title", label);
        bottom += AXIS_TITLE_ROW_H;
    }

    let (body, bounds) = canvas.into_parts();
    (
        body,
        total_w.max(bounds.width()) + CANVAS_MARGIN,
        bottom + CANVAS_MARGIN,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(name: &str, values: &[f64]) -> ChartSeries {
        ChartSeries {
            name: name.to_string(),
            values: values.to_vec(),
        }
    }

    fn cats(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn attr(tag: &str, name: &str) -> f32 {
        tag.split(&format!(r#" {name}=""#))
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0)
    }

    /// Opening tags of every `<el ... class="…{class}…">` in `svg`.
    fn tags<'a>(svg: &'a str, el: &str, class: &str) -> Vec<&'a str> {
        svg.match_indices(&format!("<{el} "))
            .map(|(start, _)| {
                let end = svg[start..].find('>').map_or(svg.len(), |i| start + i);
                &svg[start..end]
            })
            .filter(|tag| {
                tag.split(r#"class=""#)
                    .nth(1)
                    .and_then(|r| r.split('"').next())
                    .is_some_and(|c| c.split(' ').any(|t| t == class))
            })
            .collect()
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
    fn numbers_get_separators_abbreviations_and_a_true_minus() {
        let plain = NumFmt::for_range(50_000.0);
        assert_eq!(plain.value(12_345.0), "12,345");
        assert_eq!(plain.value(12.5), "12.5");
        assert_eq!(plain.value(0.3000000001), "0.3");
        assert_eq!(plain.value(-4.0), format!("{MINUS}4"));
        assert_eq!(plain.value(-0.0), "0");
        let millions = NumFmt::for_range(2_000_000.0);
        assert_eq!(millions.tick(500_000.0), "0.5M");
        assert_eq!(millions.tick(1_500_000.0), "1.5M");
        assert_eq!(millions.tick(0.0), "0");
    }

    #[test]
    fn bar_chart_renders_caller_categories_and_values() {
        let categories = cats(&["Q1", "Q2", "Q3", "Q4"]);
        let series = vec![series("Revenue", &[12.4, 15.1, 22.8, 24.0])];
        let (body, _, _) = render(
            ChartType::Bar,
            &categories,
            &series,
            None,
            Some("USD millions"),
        );
        assert!(body.contains("Q1"));
        assert!(body.contains(">24<"));
        assert!(body.contains("USD millions"));
    }

    #[test]
    fn grouped_bar_draws_one_bar_per_series_per_category() {
        let categories = cats(&["Q1", "Q2"]);
        let series = vec![series("2024", &[10.0, 20.0]), series("2025", &[15.0, 25.0])];
        let (body, _, _) = render(ChartType::GroupedBar, &categories, &series, None, None);
        // Two bars per series plus that series' legend swatch.
        assert_eq!(tags(&body, "rect", "bar-0").len(), 3);
        assert_eq!(tags(&body, "rect", "bar-1").len(), 3);
        assert!(body.contains("2024"));
        assert!(body.contains("2025"));
    }

    #[test]
    fn line_chart_draws_a_path_and_points() {
        let categories = cats(&["Jan", "Feb", "Mar"]);
        let series = vec![series("Users", &[100.0, 150.0, 130.0])];
        let (body, _, _) = render(ChartType::Line, &categories, &series, None, None);
        assert!(body.contains("series-line"));
        assert!(body.contains("data-point"));
    }

    #[test]
    fn horizontal_bar_handles_long_category_names() {
        let categories = cats(&["A very long category name indeed", "Short"]);
        let series = vec![series("Value", &[42.0, 7.0])];
        let (body, w, _) = render(ChartType::HorizontalBar, &categories, &series, None, None);
        // Wrapped onto two lines within its column, nothing dropped.
        assert!(body.contains("A very long category name"));
        assert!(body.contains(">indeed<"));
        assert!(!body.contains('\u{2026}'));
        assert!(w > 0.0);
    }

    #[test]
    fn every_bar_stays_within_canvas_bounds() {
        let categories: Vec<String> = (0..10)
            .map(|i| format!("Category {i} with a longer label"))
            .collect();
        let values: Vec<f64> = (0..10).map(|i| i as f64 * 3.5).collect();
        let series = vec![series("S", &values)];
        let (body, w, h) = render(
            ChartType::Bar,
            &categories,
            &series,
            Some("Category"),
            Some("Value"),
        );
        for tag in tags(&body, "rect", "bar") {
            let (x, y, rw, rh) = (
                attr(tag, "x"),
                attr(tag, "y"),
                attr(tag, "width"),
                attr(tag, "height"),
            );
            assert!(
                x >= 0.0 && x + rw <= w + 0.5,
                "bar leaves the canvas: {tag}"
            );
            assert!(
                y >= 0.0 && y + rh <= h + 0.5,
                "bar leaves the canvas: {tag}"
            );
        }
    }

    /// The reported bug: a category label that wrapped to two lines was
    /// centred on its anchor, so its first line climbed onto the x-axis.
    #[test]
    fn category_labels_start_below_the_axis_however_many_lines() {
        let categories = cats(&[
            "North America region",
            "Europe and Middle East",
            "Asia Pacific",
            "Latin America",
            "Rest of world",
            "Unallocated",
        ]);
        let series = vec![series("Sales", &[5.0, 9.0, 7.0, 3.0, 2.0, 1.0])];
        let (body, _, h) = render(ChartType::Bar, &categories, &series, Some("Region"), None);
        let axis_y = attr(tags(&body, "line", "axis-line")[0], "y1");
        let labels = tags(&body, "text", "cat-label");
        assert_eq!(labels.len(), categories.len());
        assert!(
            body.contains(r#"dy="14.0""#),
            "expected at least one two-line label in this fixture"
        );
        for tag in labels {
            let baseline = attr(tag, "y");
            // The glyphs' tops are about 0.75em above the baseline.
            assert!(
                baseline - LABEL_FONT_PX * 0.75 > axis_y + 2.0,
                "label rises onto the axis (axis y={axis_y}): {tag}"
            );
            assert!(baseline < h);
        }
    }

    /// The other half of it: the value-axis title used to share a baseline
    /// with the label over a bar that reached the top of the plot.
    #[test]
    fn axis_titles_and_value_labels_occupy_separate_bands() {
        let categories = cats(&["A", "B", "C"]);
        let series = vec![series("S", &[100.0, 40.0, 60.0])];
        let (body, _, _) = render(
            ChartType::Bar,
            &categories,
            &series,
            Some("Things"),
            Some("Units sold"),
        );
        let titles = tags(&body, "text", "axis-title");
        assert_eq!(titles.len(), 2);
        let y_title = attr(titles[0], "y");
        let x_title = attr(titles[1], "y");
        let value_ys: Vec<f32> = tags(&body, "text", "value-label")
            .iter()
            .map(|t| attr(t, "y"))
            .collect();
        assert_eq!(value_ys.len(), 3);
        let cat_ys: Vec<f32> = tags(&body, "text", "cat-label")
            .iter()
            .map(|t| attr(t, "y"))
            .collect();
        for vy in &value_ys {
            assert!(
                vy - VALUE_FONT_PX > y_title,
                "value label (baseline {vy}) collides with the axis title (baseline {y_title})"
            );
        }
        for cy in &cat_ys {
            assert!(x_title - LABEL_FONT_PX > *cy);
        }
    }

    #[test]
    fn tick_labels_end_short_of_the_plot() {
        let categories = cats(&["A", "B"]);
        let series = vec![series("S", &[1_250_000.0, 3_400_000.0])];
        let (body, _, _) = render(ChartType::Bar, &categories, &series, None, None);
        let plot_left = attr(tags(&body, "line", "grid-line")[0], "x1");
        for tag in tags(&body, "text", "tick-label") {
            let x = attr(tag, "x");
            assert!(
                x <= plot_left - TICK_GAP + 0.1,
                "tick label touches the plot"
            );
            let label = body[body.find(tag).unwrap() + tag.len() + 1..]
                .split('<')
                .next()
                .unwrap();
            assert!(
                x - text::measure_px(label, LABEL_FONT_PX) >= -0.5,
                "tick label {label:?} is clipped at the left edge"
            );
        }
    }

    #[test]
    fn labels_too_long_for_their_slots_are_turned_and_stay_on_the_canvas() {
        let categories: Vec<String> = (0..20)
            .map(|i| format!("Fiscal period number {i}"))
            .collect();
        let values: Vec<f64> = (0..20).map(|i| i as f64).collect();
        let (body, _, h) = render(
            ChartType::Bar,
            &categories,
            &[series("S", &values)],
            None,
            None,
        );
        let turned = tags(&body, "text", "cat-label-rot");
        assert!(!turned.is_empty(), "expected turned labels");
        assert!(tags(&body, "text", "cat-label").is_empty());
        let axis_y = attr(tags(&body, "line", "axis-line")[0], "y1");
        for (i, tag) in turned.iter().enumerate() {
            let (x, y) = (attr(tag, "x"), attr(tag, "y"));
            let w = text::measure_px(&categories[i], LABEL_FONT_PX);
            assert!(
                y - LABEL_FONT_PX * 0.75 > axis_y,
                "turned label on the axis"
            );
            assert!(
                x - w * DIAG >= -0.5,
                "turned label clipped at the left: {tag}"
            );
            assert!(
                y + w * DIAG <= h,
                "turned label clipped at the bottom: {tag}"
            );
        }
    }

    #[test]
    fn value_labels_are_dropped_when_they_would_collide() {
        let categories: Vec<String> = (0..12).map(|i| format!("M{i}")).collect();
        let a: Vec<f64> = (0..12).map(|i| 123_456.0 + i as f64).collect();
        let series = vec![series("A", &a), series("B", &a), series("C", &a)];
        let (body, _, _) = render(ChartType::GroupedBar, &categories, &series, None, None);
        assert!(tags(&body, "text", "value-label").is_empty());
    }

    #[test]
    fn horizontal_bar_names_and_value_labels_keep_clear_of_the_bars() {
        let categories = cats(&["Research and development", "Sales", "Ops"]);
        let series = vec![series("Change", &[42.0, -17.0, 8.0])];
        let (body, w, _) = render(ChartType::HorizontalBar, &categories, &series, None, None);
        let bars = tags(&body, "rect", "bar");
        let bars_left = bars.iter().map(|t| attr(t, "x")).fold(f32::MAX, f32::min);
        for (i, tag) in tags(&body, "text", "cat-label-start").iter().enumerate() {
            let right = attr(tag, "x") + text::measure_px(&categories[i], LABEL_FONT_PX);
            assert!(right < bars_left, "category name reaches the bars: {tag}");
        }
        for tag in tags(&body, "text", "value-label-start") {
            assert!(attr(tag, "x") + text::measure_px("42", VALUE_FONT_PX) <= w);
        }
        let negative = tags(&body, "text", "value-label-end");
        assert_eq!(negative.len(), 1);
        let names_right = text::measure_px("Research and development", LABEL_FONT_PX);
        let label_left =
            attr(negative[0], "x") - text::measure_px(&format!("{MINUS}17"), VALUE_FONT_PX);
        assert!(
            label_left > names_right,
            "negative value label lands on the names"
        );
    }
}
