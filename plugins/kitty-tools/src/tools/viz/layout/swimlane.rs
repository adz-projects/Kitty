//! `swimlane` — who does what: steps grouped into horizontal actor bands.
//! Lanes come entirely from the caller's `lane` values now; the crate used to
//! hard-code a fixed 4-lane e-commerce checkout ("Submit Cart" -> "Persist
//! Record") regardless of input.
//!
//! Unlike `single_lane`, this layout can't wrap to additional rows without
//! destroying the lane semantics (a step's vertical position *is* meaning
//! here), so it gets the tightest node cap of any diagram_type
//! (`model::MAX_NODES_SWIMLANE`) instead.

use std::collections::HashMap;

use crate::tools::viz::layout::{draw_node, size_node_capped, NodeVisual, GAP_X, MAX_CONTENT_W_WIDE, MAX_NODE_W, MIN_LAYER_GAP, MIN_NODE_W};
use crate::tools::viz::model::{Step, StepType};
use crate::tools::viz::render::svg::{SvgCanvas, CANVAS_MARGIN, TITLE_BAND};
use crate::tools::viz::text;

const LEFT_MARGIN: f32 = 12.0;
const MIN_GUTTER: f32 = 110.0;
const LANE_HEADER_FONT_PX: f32 = 11.0;
const BAND_PAD: f32 = 24.0;
const MIN_BAND_H: f32 = 90.0;
/// Reserved strip at the top of every lane band for its header text. Nodes are
/// vertically centered in the region *below* this strip, so a tall node can
/// never overdraw the lane name (the old behavior: `band_top+22` header vs. a
/// 3-line node whose top edge sat at `band_top+12`).
const LANE_HEADER_H: f32 = 26.0;

fn lane_key(step: &Step) -> String {
    step.lane.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("Unassigned").to_string()
}

pub fn render(steps: &[Step]) -> (String, f32, f32) {
    let n = steps.len();

    let mut lanes_order: Vec<String> = Vec::new();
    let mut lane_index: HashMap<String, usize> = HashMap::new();
    for step in steps {
        let key = lane_key(step);
        if !lane_index.contains_key(&key) {
            lane_index.insert(key.clone(), lanes_order.len());
            lanes_order.push(key);
        }
    }

    let gutter = lanes_order
        .iter()
        .map(|name| text::measure_px(&name.to_uppercase(), LANE_HEADER_FONT_PX) + BAND_PAD)
        .fold(MIN_GUTTER, f32::max);

    // Readability compression: a swimlane can't wrap to extra rows without
    // destroying lane semantics, so fit `MAX_CONTENT_W_WIDE` by tightening the
    // inter-node gap first, then shrinking the node-width cap. Past the cap
    // floor the centralized check in `viz/mod.rs` returns `VIZ_TOO_WIDE`.
    let mut cap = MAX_NODE_W;
    let mut gap = GAP_X;
    let mut sized: Vec<_> = steps.iter().map(|s| size_node_capped(&s.text, None, cap)).collect();
    for _ in 0..6 {
        let total = gutter + LEFT_MARGIN + sized.iter().map(|s| s.w).sum::<f32>() + gap * (n as f32 - 1.0).max(0.0);
        if total <= MAX_CONTENT_W_WIDE {
            break;
        }
        if gap > MIN_LAYER_GAP + 1.0 {
            gap = (gap / 2.0).max(MIN_LAYER_GAP);
            continue;
        }
        let nodes_sum: f32 = sized.iter().map(|s| s.w).sum();
        cap = (cap * (MAX_CONTENT_W_WIDE - gutter - LEFT_MARGIN - MIN_LAYER_GAP * (n as f32 - 1.0)) / nodes_sum).clamp(MIN_NODE_W, MAX_NODE_W);
        sized = steps.iter().map(|s| size_node_capped(&s.text, None, cap)).collect();
    }

    let mut band_h = vec![MIN_BAND_H; lanes_order.len()];
    for (i, step) in steps.iter().enumerate() {
        let k = lane_index[&lane_key(step)];
        band_h[k] = band_h[k].max(sized[i].h + BAND_PAD + LANE_HEADER_H);
    }
    let mut band_top = vec![0.0f32; lanes_order.len()];
    let mut acc = TITLE_BAND;
    for k in 0..lanes_order.len() {
        band_top[k] = acc;
        acc += band_h[k];
    }

    // Every step advances monotonically along x regardless of lane -- this is
    // what makes a swimlane readable as "time flows right", the same idiom
    // the crate's original static asset used.
    let mut x = vec![0.0f32; n];
    let mut cursor = gutter + LEFT_MARGIN;
    for i in 0..n {
        x[i] = cursor;
        cursor += sized[i].w + gap;
    }

    let mut canvas = SvgCanvas::new();

    let mut top_y = vec![0.0f32; n];
    for (i, step) in steps.iter().enumerate() {
        let k = lane_index[&lane_key(step)];
        // Center nodes in the region below the header strip so the tallest
        // node still starts at least LANE_HEADER_H below the band top.
        let body_top = band_top[k] + LANE_HEADER_H;
        let body_h = band_h[k] - LANE_HEADER_H;
        let center_y = body_top + body_h / 2.0;
        top_y[i] = center_y - sized[i].h / 2.0;
    }

    for k in 0..lanes_order.len() {
        canvas.text_line(8.0, band_top[k] + 22.0, "lane-header", &lanes_order[k].to_uppercase());
        if k + 1 < lanes_order.len() {
            let divider_y = band_top[k] + band_h[k];
            canvas.line(0.0, divider_y, cursor, divider_y, "lane-divider");
        }
    }

    for i in 0..n.saturating_sub(1) {
        let (x1, y1) = (x[i] + sized[i].w, top_y[i] + sized[i].h / 2.0);
        let (x2, y2) = (x[i + 1], top_y[i + 1] + sized[i + 1].h / 2.0);
        let mid_x = (x1 + x2) / 2.0;
        let d = format!("M {x1:.1},{y1:.1} L {mid_x:.1},{y1:.1} L {mid_x:.1},{y2:.1} L {x2:.1},{y2:.1}");
        let bbox = (x1.min(x2), y1.min(y2), (x2 - x1).abs(), (y2 - y1).abs());
        canvas.path(&d, "flow-path", bbox);
    }

    for (i, node) in sized.iter().enumerate() {
        draw_node(&mut canvas, x[i], top_y[i], node.w, node.h, NodeVisual { lines: &node.lines, badge: None, step_type: StepType::Process });
    }

    // Reserve the full lane grid explicitly: a lane whose only node is
    // shorter than its band (because some other lane's node set the band
    // height) would otherwise leave the band's bottom edge and the final
    // divider's row untracked by any drawn primitive's own bounds.
    canvas.reserve(0.0, 0.0, cursor, acc);

    let (body, bounds) = canvas.into_parts();
    (body, bounds.width() + CANVAS_MARGIN, bounds.height() + CANVAS_MARGIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(text: &str, lane: &str) -> Step {
        Step { text: text.to_string(), lane: Some(lane.to_string()), ..Default::default() }
    }

    #[test]
    fn renders_caller_supplied_lanes_and_steps() {
        let steps = vec![step("Place order", "Customer"), step("Charge card", "Payments"), step("Ship item", "Warehouse")];
        let (body, _, _) = render(&steps);
        assert!(body.contains("CUSTOMER"));
        assert!(body.contains("PAYMENTS"));
        assert!(body.contains("WAREHOUSE"));
        assert!(body.contains("Place order"));
        assert!(!body.contains("Submit Cart"), "must not fall back to the retired e-commerce clipart");
        assert!(!body.contains("BACKEND API"), "must not fall back to the retired e-commerce clipart");
    }

    #[test]
    fn steps_without_a_lane_get_an_unassigned_band_instead_of_panicking() {
        let steps = vec![step("A", "Team A"), Step { text: "No lane here".to_string(), ..Default::default() }];
        let (body, _, _) = render(&steps);
        assert!(body.contains("UNASSIGNED"));
    }

    #[test]
    fn every_node_and_divider_stays_within_canvas_bounds() {
        let steps: Vec<Step> = (0..8).map(|i| step(&format!("Step {i}"), if i % 2 == 0 { "Customer" } else { "Backend" })).collect();
        let (body, w, h) = render(&steps);
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
