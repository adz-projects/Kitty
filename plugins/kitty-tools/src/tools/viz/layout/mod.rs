//! Shared node-sizing constants and the one function every `diagram_type`
//! layout uses to turn a label into a box that actually fits it — the direct
//! fix for the crate's old fixed-130px-node behavior, where anything past
//! ~15 characters silently overflowed its box.

pub mod chart;
pub mod graph;
pub mod journey;
pub mod linear;
pub mod swimlane;

use super::model::StepType;
use super::render::svg::SvgCanvas;
use super::text;

pub const MIN_NODE_W: f32 = 110.0;
pub const MAX_NODE_W: f32 = 220.0;
pub const PAD_X: f32 = 14.0;
pub const PAD_Y: f32 = 12.0;
/// Matches the 12.5px `.node-text` rule in `assets/defs.svg`.
pub const NODE_FONT_PX: f32 = 12.5;
pub const LINE_H: f32 = 15.0;
/// Matches the 8.5px `.badge-meta` rule in `assets/defs.svg`.
pub const BADGE_FONT_PX: f32 = 8.5;
pub const MAX_LINES: usize = 3;
pub const MIN_NODE_H: f32 = 40.0;
pub const GAP_X: f32 = 32.0;
pub const GAP_Y: f32 = 56.0;

/// Decision (triangle) nodes: a triangle narrows to a point at its apex, so it
/// offers far less label room than its bounding box implies. These constants
/// keep the label inside the widest (lower) band of the triangle by wrapping
/// narrower, bottom-anchoring the block, and reserving enough height that the
/// topmost line still sits where the triangle is wide enough. `0.60` label
/// width vs. `~0.675` available at the top line (see `size_decision`) leaves a
/// built-in margin so the `textLength` backstop never has to squeeze text.
pub const DECISION_MIN_W: f32 = 150.0;
pub const DECISION_MIN_H: f32 = 120.0;
pub const DECISION_LABEL_FRAC: f32 = 0.60;
pub const DECISION_BADGE_FRAC: f32 = 0.22;
pub const DECISION_BASE_MARGIN: f32 = 18.0;

/// Readability budget: the chat iframe renders these SVGs at ~its own width
/// (`svg { width:100% }` in `assets/wrapper.html`), so any diagram wider than
/// this shrinks its text below legible size. Layouts must fit within the budget
/// (wrapping rows, compressing gaps, reducing node widths) or the caller gets a
/// `VIZ_TOO_WIDE` error instead of an unreadable render. Swimlane/journey can't
/// wrap without destroying their semantics, so they get a slightly larger
/// budget and rely on `VIZ_TOO_WIDE` past it.
pub const MAX_CONTENT_W: f32 = 1100.0;
pub const MAX_CONTENT_W_WIDE: f32 = 1500.0;
/// Minimum inter-node gap during horizontal compression (below this a layer is
/// too dense and node widths get reduced instead).
pub const MIN_LAYER_GAP: f32 = 12.0;
/// Absorbs `CANVAS_MARGIN` plus the linear layout's serpentine overshoot when
/// the centralized width check runs.
pub const WIDTH_SLACK: f32 = 60.0;

/// Horizontally caps node sizing at `max_w` instead of `MAX_NODE_W` — the
/// primitive layout compression uses to fit wide diagrams into `MAX_CONTENT_W`.
/// Non-decision nodes wrap labels to the capped inner width; decisions clamp
/// their box and triangle label band to it.
pub fn size_node_capped(label: &str, badge: Option<&str>, max_w: f32) -> SizedNode {
    let label = if label.trim().is_empty() { "Untitled".to_string() } else { label.to_string() };

    if badge.is_some() {
        return size_decision_capped(&label, badge, max_w);
    }

    let inner_w = (max_w - 2.0 * PAD_X).max(MIN_NODE_W - 2.0 * PAD_X);

    let (lines, content_w) = if text::measure_px(&label, NODE_FONT_PX) <= inner_w {
        let w = text::measure_px(&label, NODE_FONT_PX);
        (vec![label], w)
    } else {
        let wrapped = text::wrap(&label, inner_w, NODE_FONT_PX, MAX_LINES);
        let widest = wrapped.iter().map(|l| text::measure_px(l, NODE_FONT_PX)).fold(0.0_f32, f32::max);
        (wrapped, widest)
    };

    let w = (content_w + 2.0 * PAD_X).clamp(MIN_NODE_W, max_w);

    let h = (2.0 * PAD_Y + lines.len() as f32 * LINE_H).max(MIN_NODE_H);

    SizedNode { w, h, lines, badge: None }
}

/// `size_decision` capped at `max_w` (see `size_node_capped`). Iterates to
/// convergence because the wrap budget depends on the final node width, which
/// depends on the wrapped content: wrap against a first estimate, recompute the
/// width, re-wrap against the (narrower) band until stable. The loop is bounded
/// because widths only shrink and are floored at `DECISION_MIN_W`.
fn size_decision_capped(label: &str, badge: Option<&str>, max_w: f32) -> SizedNode {
    let label = label.to_string();
    let mut w = (text::measure_px(&label, NODE_FONT_PX) + 2.0 * PAD_X).clamp(DECISION_MIN_W, max_w);
    let mut lines = Vec::new();
    for _ in 0..4 {
        let band = DECISION_LABEL_FRAC * w;
        let (new_lines, new_content) = if text::measure_px(&label, NODE_FONT_PX) <= band {
            let m = text::measure_px(&label, NODE_FONT_PX);
            (vec![label.clone()], m)
        } else {
            let wrapped = text::wrap(&label, band, NODE_FONT_PX, 2);
            let widest = wrapped.iter().map(|l| text::measure_px(l, NODE_FONT_PX)).fold(0.0_f32, f32::max);
            (wrapped, widest)
        };
        let new_w = (new_content + 2.0 * PAD_X).clamp(DECISION_MIN_W, max_w);
        let stable = new_lines == lines && (new_w - w).abs() < 0.5;
        lines = new_lines;
        w = new_w;
        if stable {
            break;
        }
    }

    let badge = badge.and_then(|b| {
        let max_badge_w = (DECISION_BADGE_FRAC * w).max(10.0);
        text::wrap(b, max_badge_w, BADGE_FONT_PX, 1).into_iter().next()
    });

    SizedNode { w, h: DECISION_MIN_H, lines, badge }
}

pub struct SizedNode {
    pub w: f32,
    pub h: f32,
    pub lines: Vec<String>,
    pub badge: Option<String>,
}

/// Sizes a node with the crate's default width cap: single-line if the label
/// fits within `MAX_NODE_W`, otherwise wrapped up to `MAX_LINES` (and ellipsized
/// on overflow past that — see `text::wrap`). `badge`, if given, marks a
/// decision node and routes to the triangle-aware sizing instead.
pub fn size_node(label: &str, badge: Option<&str>) -> SizedNode {
    size_node_capped(label, badge, MAX_NODE_W)
}

/// The label/badge/shape inputs to `draw_node`, grouped into one struct
/// purely to keep that function's argument count clippy-friendly — every
/// caller already has these three sourced together from a `SizedNode` plus
/// the originating step's `step_type`.
pub struct NodeVisual<'a> {
    pub lines: &'a [String],
    pub badge: Option<&'a str>,
    pub step_type: StepType,
}

/// Draws one node at absolute top-left `(x, y)` with size `(w, h)`. Shape
/// follows `visual.step_type`: "start"/"end" get the pill terminator,
/// "decision" gets the triangular gate shape (matching the crate's original
/// visual language for gate nodes), everything else gets the plain box.
/// `visual.lines` are already-wrapped label text from `size_node`;
/// `visual.badge` (only meaningful on a "decision" node — see the schema doc
/// comment on `subtitle`) is a short caption drawn above the label.
pub fn draw_node(canvas: &mut SvgCanvas, x: f32, y: f32, w: f32, h: f32, visual: NodeVisual) {
    let cx = x + w / 2.0;
    match visual.step_type {
        StepType::Decision => {
            canvas.polygon(&[(cx, y), (x + w, y + h), (x, y + h)], "node-triangle");
            if let Some(b) = visual.badge {
                // Badge just below the apex, where the triangle is wide enough
                // for a short caption; `text_line_fit` clamps it to the
                // available band so it can never spill past the slanted edges.
                canvas.text_line_fit(cx, y + h * 0.26, "badge-meta", b, BADGE_FONT_PX, DECISION_BADGE_FRAC * w);
            }
            // Bottom-anchored label: the block is centered so its last line
            // sits DECISION_BASE_MARGIN above the base, keeping every line in
            // the triangle's wide lower band. `size_decision` guaranteed each
            // line fits that band, so the textLength backstop stays idle.
            let n = visual.lines.len() as f32;
            let block_center = (y + h - DECISION_BASE_MARGIN) - (n - 1.0) * LINE_H / 2.0;
            canvas.text_lines_fit(cx, block_center, "node-text", visual.lines, LINE_H, NODE_FONT_PX, DECISION_LABEL_FRAC * w);
        }
        StepType::Start | StepType::End => {
            canvas.rect(x, y, w, h, "node-box pill");
            canvas.text_lines_fit(cx, y + h / 2.0, "node-text", visual.lines, LINE_H, NODE_FONT_PX, w - 2.0 * PAD_X);
        }
        StepType::Process => {
            canvas.rect(x, y, w, h, "node-box");
            canvas.text_lines_fit(cx, y + h / 2.0, "node-text", visual.lines, LINE_H, NODE_FONT_PX, w - 2.0 * PAD_X);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_label_is_single_line_and_min_width() {
        let n = size_node("OK", None);
        assert_eq!(n.lines.len(), 1);
        assert_eq!(n.w, MIN_NODE_W);
    }

    #[test]
    fn long_label_wraps_and_caps_width() {
        let n = size_node("This is a much longer label than any single line node should hold comfortably", None);
        assert!(n.lines.len() > 1);
        assert!(n.w <= MAX_NODE_W);
        assert!(n.lines.len() <= MAX_LINES);
    }

    #[test]
    fn empty_label_falls_back_to_placeholder() {
        let n = size_node("   ", None);
        assert_eq!(n.lines, vec!["Untitled".to_string()]);
    }

    #[test]
    fn badge_present_increases_height() {
        let without = size_node("Step", None);
        let with = size_node("Step", Some("GATE"));
        assert!(with.h > without.h);
    }

    #[test]
    fn every_node_line_fits_inside_its_own_width() {
        let n = size_node("An extremely long process step description that must wrap several times over", None);
        let inner_w = n.w - 2.0 * PAD_X;
        for line in &n.lines {
            assert!(text::measure_px(line, NODE_FONT_PX) <= inner_w + 0.5);
        }
    }

    #[test]
    fn every_decision_label_line_fits_the_triangle_band() {
        // Constructional guarantee behind the decision-triangle fix: labels wrap
        // at 0.60*w, and the bottom-anchored block's top line sits at
        // t = (h - DECISION_BASE_MARGIN - (n-1)*LINE_H)/h where the triangle
        // offers t*w of width. Assert both directly so a constant drift fails
        // loudly instead of silently re-introducing apex spill.
        for label in ["Payment approved?", "Are all credentials valid before we proceed to the next step?", "OK", "x".repeat(60).as_str()] {
            let n = size_node(label, Some("GATE"));
            let block_clearance = DECISION_BASE_MARGIN + (n.lines.len() as f32 - 1.0) * LINE_H;
            let top_line_t = (n.h - block_clearance) / n.h;
            let available = top_line_t * n.w;
            for line in &n.lines {
                let w = text::measure_px(line, NODE_FONT_PX);
                assert!(
                    w <= 0.60 * n.w + 0.5,
                    "{label:?}: line {line:?} ({w:.1}px) exceeds the 0.60 wrap budget ({:.1}px)",
                    0.60 * n.w
                );
                assert!(
                    w <= available + 0.5,
                    "{label:?}: line {line:?} ({w:.1}px) exceeds the {available:.1}px available at the block's top line"
                );
            }
        }
    }
}
