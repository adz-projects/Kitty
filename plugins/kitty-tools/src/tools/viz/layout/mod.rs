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
/// Matches the 13px `.node-text` rule in the host stylesheet
/// (`src/themes/viz.css` in the app).
pub const NODE_FONT_PX: f32 = 13.0;
pub const LINE_H: f32 = 16.0;
/// Matches the 9.5px `.badge-meta` rule in the host stylesheet.
pub const BADGE_FONT_PX: f32 = 9.5;
pub const MAX_LINES: usize = 3;
pub const MIN_NODE_H: f32 = 40.0;
pub const GAP_X: f32 = 32.0;
pub const GAP_Y: f32 = 56.0;

/// Decision nodes are diamonds: a diamond is only as wide as its box at the
/// middle and narrows to a point above and below, so it offers far less label
/// room than its bounding box implies. The label is wrapped to `0.60` of the
/// width and set as a block centred `DECISION_LABEL_DROP` below the middle
/// (leaving the upper half for the caption); with two lines the block's outer
/// edge is ~0.57 of the height from a vertex, so the `textLength` backstop
/// never has to squeeze text. `layout::tests` asserts that arithmetic directly.
pub const DECISION_MIN_W: f32 = 160.0;
pub const DECISION_MIN_H: f32 = 112.0;
pub const DECISION_LABEL_FRAC: f32 = 0.60;
pub const DECISION_BADGE_FRAC: f32 = 0.30;
/// How far below the diamond's middle the label block is centred when the
/// node also carries a caption.
pub const DECISION_LABEL_DROP: f32 = 6.0;
/// Caption baseline, as a fraction of the height from the top vertex.
pub const DECISION_BADGE_AT: f32 = 0.29;

/// Readability budget: the chat shows these SVGs at their natural size and
/// only ever scales them down (never up) to fit its column, so any diagram much
/// wider than this shrinks its text below legible size. Layouts must fit within
/// the budget (wrapping rows, compressing gaps, reducing node widths) or the
/// caller gets a `VIZ_TOO_WIDE` error instead of an unreadable render.
/// Swimlane/journey can't wrap without destroying their semantics, so they get
/// a slightly larger budget and rely on `VIZ_TOO_WIDE` past it.
/// 960px shown in a ~670px chat column is a 0.7 scale: 13px type at ~9px,
/// the smallest that still reads. The host scrolls sideways past that scale
/// rather than shrinking further (`MIN_SCALE` in the app's `lib/viz.ts`).
pub const MAX_CONTENT_W: f32 = 960.0;
pub const MAX_CONTENT_W_WIDE: f32 = 1300.0;
/// The width layouts *aim* for: the chat column, where a figure is shown at
/// its natural size. Compression (tighter gaps, narrower nodes with wrapped
/// labels) starts here rather than at the budgets above, because a taller
/// diagram at full size reads better than a wider one scaled down. A diagram
/// that cannot be compressed this far is still accepted up to its budget.
pub const COMFORT_W: f32 = 660.0;
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
    let label = if label.trim().is_empty() {
        "Untitled".to_string()
    } else {
        label.to_string()
    };

    if badge.is_some() {
        return size_decision_capped(&label, badge, max_w);
    }

    let inner_w = (max_w - 2.0 * PAD_X).max(MIN_NODE_W - 2.0 * PAD_X);

    let (lines, content_w) = if text::measure_px(&label, NODE_FONT_PX) <= inner_w {
        let w = text::measure_px(&label, NODE_FONT_PX);
        (vec![label], w)
    } else {
        let wrapped = text::wrap(&label, inner_w, NODE_FONT_PX, MAX_LINES);
        let widest = wrapped
            .iter()
            .map(|l| text::measure_px(l, NODE_FONT_PX))
            .fold(0.0_f32, f32::max);
        (wrapped, widest)
    };

    let w = (content_w + 2.0 * PAD_X).clamp(MIN_NODE_W, max_w);

    let h = (2.0 * PAD_Y + lines.len() as f32 * LINE_H).max(MIN_NODE_H);

    SizedNode {
        w,
        h,
        lines,
        badge: None,
    }
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
            let widest = wrapped
                .iter()
                .map(|l| text::measure_px(l, NODE_FONT_PX))
                .fold(0.0_f32, f32::max);
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
        text::wrap(b, max_badge_w, BADGE_FONT_PX, 1)
            .into_iter()
            .next()
    });

    SizedNode {
        w,
        h: DECISION_MIN_H,
        lines,
        badge,
    }
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
/// follows `visual.step_type`: "start"/"end" get the solid terminator (class
/// `pill`, with its label in the `inverse` text class), "decision" gets a
/// diamond whose four vertices are the midpoints of the box's sides -- so the
/// connectors every layout already aims at those midpoints land on a vertex --
/// and everything else gets the plain box.
/// `visual.lines` are already-wrapped label text from `size_node`;
/// `visual.badge` (only meaningful on a "decision" node — see the schema doc
/// comment on `subtitle`) is a short caption drawn above the label.
pub fn draw_node(canvas: &mut SvgCanvas, x: f32, y: f32, w: f32, h: f32, visual: NodeVisual) {
    let cx = x + w / 2.0;
    match visual.step_type {
        StepType::Decision => {
            let cy = y + h / 2.0;
            canvas.polygon(
                &[(cx, y), (x + w, cy), (cx, y + h), (x, cy)],
                "node-diamond",
            );
            if let Some(b) = visual.badge {
                // Caption in the upper half, where the diamond is already
                // wider than the caption's band; `text_line_fit` clamps it so
                // it can never spill past the slanted edges.
                canvas.text_line_fit(
                    cx,
                    y + h * DECISION_BADGE_AT,
                    "badge-meta",
                    b,
                    BADGE_FONT_PX,
                    DECISION_BADGE_FRAC * w,
                );
            }
            let drop = if visual.badge.is_some() {
                DECISION_LABEL_DROP
            } else {
                0.0
            };
            canvas.text_lines_fit(
                cx,
                cy + drop,
                "node-text",
                visual.lines,
                LINE_H,
                NODE_FONT_PX,
                DECISION_LABEL_FRAC * w,
            );
        }
        StepType::Start | StepType::End => {
            canvas.rect(x, y, w, h, "node-box pill");
            canvas.text_lines_fit(
                cx,
                y + h / 2.0,
                "node-text inverse",
                visual.lines,
                LINE_H,
                NODE_FONT_PX,
                w - 2.0 * PAD_X,
            );
        }
        StepType::Process => {
            canvas.rect(x, y, w, h, "node-box");
            canvas.text_lines_fit(
                cx,
                y + h / 2.0,
                "node-text",
                visual.lines,
                LINE_H,
                NODE_FONT_PX,
                w - 2.0 * PAD_X,
            );
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
        let n = size_node(
            "This is a much longer label than any single line node should hold comfortably",
            None,
        );
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
        let n = size_node(
            "An extremely long process step description that must wrap several times over",
            None,
        );
        let inner_w = n.w - 2.0 * PAD_X;
        for line in &n.lines {
            assert!(text::measure_px(line, NODE_FONT_PX) <= inner_w + 0.5);
        }
    }

    #[test]
    fn every_decision_label_line_fits_inside_the_diamond() {
        // Constructional guarantee behind the diamond: labels wrap at 0.60*w,
        // and a diamond's width at vertical distance d from its middle is
        // w * (1 - 2d/h). The label block is centred DECISION_LABEL_DROP below
        // the middle, so the baseline of its outermost line is half the block
        // height (less the descender room) plus that drop away. Assert both
        // directly so a constant drift fails loudly instead of silently
        // letting text cross the slanted edges.
        for label in [
            "Payment approved?",
            "Are all credentials valid before we proceed to the next step?",
            "OK",
            "x".repeat(60).as_str(),
        ] {
            let n = size_node(label, Some("GATE"));
            let lines = n.lines.len() as f32;
            let furthest = (lines - 1.0) * LINE_H / 2.0 + NODE_FONT_PX * 0.4 + DECISION_LABEL_DROP;
            let available = n.w * (1.0 - 2.0 * furthest / n.h);
            for line in &n.lines {
                let w = text::measure_px(line, NODE_FONT_PX);
                assert!(
                    w <= DECISION_LABEL_FRAC * n.w + 0.5,
                    "{label:?}: line {line:?} ({w:.1}px) exceeds the wrap budget ({:.1}px)",
                    DECISION_LABEL_FRAC * n.w
                );
                assert!(
                    w <= available + 0.5,
                    "{label:?}: line {line:?} ({w:.1}px) exceeds the {available:.1}px the diamond offers at the block's edge"
                );
            }
            // The caption must clear the top of the label block.
            let caption_base = n.h * DECISION_BADGE_AT;
            let label_top =
                n.h / 2.0 + DECISION_LABEL_DROP - (lines - 1.0) * LINE_H / 2.0 - NODE_FONT_PX * 0.4;
            assert!(
                caption_base <= label_top,
                "{label:?}: caption (baseline {caption_base:.1}) sits on the label (top {label_top:.1})"
            );
        }
    }
}
