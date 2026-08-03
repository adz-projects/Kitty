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
pub const BADGE_BLOCK_H: f32 = 16.0;
pub const MAX_LINES: usize = 3;
pub const MIN_NODE_H: f32 = 40.0;
pub const GAP_X: f32 = 40.0;
pub const GAP_Y: f32 = 70.0;

pub struct SizedNode {
    pub w: f32,
    pub h: f32,
    pub lines: Vec<String>,
    pub badge: Option<String>,
}

/// Sizes a node: single-line if the label fits within `MAX_NODE_W`, otherwise
/// wrapped up to `MAX_LINES` (and ellipsized on overflow past that — see
/// `text::wrap`). `badge`, if given, is a short caption drawn above the label,
/// clamped (not wrapped) to the node's own width.
pub fn size_node(label: &str, badge: Option<&str>) -> SizedNode {
    let label = if label.trim().is_empty() { "Untitled".to_string() } else { label.to_string() };
    let inner_w = MAX_NODE_W - 2.0 * PAD_X;

    let (lines, content_w) = if text::measure_px(&label, NODE_FONT_PX) <= inner_w {
        let w = text::measure_px(&label, NODE_FONT_PX);
        (vec![label], w)
    } else {
        let wrapped = text::wrap(&label, inner_w, NODE_FONT_PX, MAX_LINES);
        let widest = wrapped.iter().map(|l| text::measure_px(l, NODE_FONT_PX)).fold(0.0_f32, f32::max);
        (wrapped, widest)
    };

    let w = (content_w + 2.0 * PAD_X).clamp(MIN_NODE_W, MAX_NODE_W);

    let badge_h = if badge.is_some() { BADGE_BLOCK_H } else { 0.0 };
    let badge = badge.and_then(|b| {
        let max_badge_w = (w - 8.0).max(10.0);
        text::wrap(b, max_badge_w, BADGE_FONT_PX, 1).into_iter().next()
    });

    let h = (2.0 * PAD_Y + lines.len() as f32 * LINE_H + badge_h).max(MIN_NODE_H);

    SizedNode { w, h, lines, badge }
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
                canvas.text_line(cx, y + h * 0.34, "badge-meta", b);
            }
            canvas.text_lines(cx, y + h * 0.72, "node-text", visual.lines, LINE_H);
        }
        StepType::Start | StepType::End => {
            canvas.rect(x, y, w, h, "node-box pill");
            canvas.text_lines(cx, y + h / 2.0, "node-text", visual.lines, LINE_H);
        }
        StepType::Process => {
            canvas.rect(x, y, w, h, "node-box");
            canvas.text_lines(cx, y + h / 2.0, "node-text", visual.lines, LINE_H);
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
}
