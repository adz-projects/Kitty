//! `single_lane` — a straight sequential process, A -> B -> C. Wraps to
//! additional rows instead of growing width without bound, since
//! `assets/wrapper.html` scales the SVG to fill the iframe's width: an
//! 880px-wide diagram scales down cleanly, but a several-thousand-pixel-wide
//! one renders its text down to a few illegible pixels. This is the direct
//! fix for the old fixed `viewBox="0 0 880 220"`, which silently clipped
//! anything past ~5 steps with no error and no visual warning.

use crate::tools::viz::layout::{draw_node, size_node, NodeVisual, GAP_X, GAP_Y, MIN_NODE_H};
use crate::tools::viz::model::{Step, StepType};
use crate::tools::viz::render::svg::{SvgCanvas, CANVAS_MARGIN, TITLE_BAND};

/// Content width budget per row before starting a new one.
const ROW_MAX_W: f32 = 1100.0;
const LEFT_X: f32 = 20.0;

struct Placed {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    row: usize,
}

/// Renders the body fragment (everything below the title bar `document()`
/// draws) plus its total width/height including `CANVAS_MARGIN`.
pub fn render(steps: &[Step]) -> (String, f32, f32) {
    let sized: Vec<_> = steps
        .iter()
        .map(|s| {
            let badge = (s.step_type == StepType::Decision).then(|| s.subtitle.clone().unwrap_or_else(|| "GATE".to_string()));
            size_node(&s.text, badge.as_deref())
        })
        .collect();

    let row_h = sized.iter().map(|n| n.h).fold(MIN_NODE_H, f32::max);

    // Pack nodes into rows first (pure arithmetic, no drawing) so connector
    // drawing below knows up front which pairs are same-row vs. row-wrapping.
    let mut row_of = vec![0usize; sized.len()];
    let mut row_w = 0.0_f32;
    let mut row_idx = 0usize;
    for (i, n) in sized.iter().enumerate() {
        let added = if row_w == 0.0 { n.w } else { GAP_X + n.w };
        if row_w > 0.0 && row_w + added > ROW_MAX_W {
            row_idx += 1;
            row_w = 0.0;
        }
        row_of[i] = row_idx;
        row_w += if row_w == 0.0 { n.w } else { GAP_X + n.w };
    }

    let mut placed: Vec<Placed> = Vec::with_capacity(sized.len());
    let mut x = LEFT_X;
    let mut current_row = 0usize;
    for (i, n) in sized.iter().enumerate() {
        if row_of[i] != current_row {
            current_row = row_of[i];
            x = LEFT_X;
        }
        let row_top = TITLE_BAND + current_row as f32 * (row_h + GAP_Y);
        let node_top = row_top + (row_h - n.h) / 2.0;
        placed.push(Placed { x, y: node_top, w: n.w, h: n.h, row: current_row });
        x += n.w + GAP_X;
    }

    let mut canvas = SvgCanvas::new();

    // Connectors first so node boxes render on top of the arrow tails.
    for i in 0..placed.len().saturating_sub(1) {
        let a = &placed[i];
        let b = &placed[i + 1];
        let ay = a.y + a.h / 2.0;
        let by = b.y + b.h / 2.0;
        if a.row == b.row {
            canvas.line(a.x + a.w, ay, b.x, by, "flow-path");
        } else {
            // Serpentine connector: loop right past the wider of the two rows'
            // content, then back down/left into the next row's first node.
            let turn_x = (a.x + a.w).max(b.x) + 40.0;
            let d = format!("M {:.1},{:.1} C {:.1},{:.1} {:.1},{:.1} {:.1},{:.1}", a.x + a.w, ay, turn_x, ay, turn_x, by, b.x, by);
            let bbox = (a.x + a.w, ay.min(by), (turn_x - (a.x + a.w)).max(0.0), (ay - by).abs());
            canvas.path(&d, "flow-path", bbox);
        }
    }

    for (i, n) in sized.iter().enumerate() {
        let p = &placed[i];
        draw_node(&mut canvas, p.x, p.y, p.w, p.h, NodeVisual { lines: &n.lines, badge: n.badge.as_deref(), step_type: steps[i].step_type });
    }

    let (body, bounds) = canvas.into_parts();
    (body, bounds.width() + CANVAS_MARGIN, bounds.height() + CANVAS_MARGIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(text: &str) -> Step {
        Step { text: text.to_string(), ..Default::default() }
    }

    #[test]
    fn viewbox_grows_monotonically_with_step_count() {
        let mut prev_w = 0.0_f32;
        let mut prev_h = 0.0_f32;
        for n in [1usize, 3, 6, 10, 20] {
            let steps: Vec<Step> = (0..n).map(|i| step(&format!("Step {i}"))).collect();
            let (_, w, h) = render(&steps);
            assert!(w >= prev_w, "width should not shrink as steps grow ({n} steps)");
            // Height only needs to grow once rows wrap; width growth alone
            // satisfies "the diagram accommodates more content" for small n.
            prev_w = w;
            prev_h = prev_h.max(h);
        }
        // With 20 steps at ~150px each, wrapping must have occurred, so
        // height must have grown past a single row.
        let (_, _, h20) = render(&(0..20).map(|i| step(&format!("Step {i}"))).collect::<Vec<_>>());
        assert!(h20 > 220.0, "20 steps must wrap to more than one row");
    }

    #[test]
    fn nine_steps_do_not_clip_off_canvas() {
        // The historical bug: >5 steps on the old fixed 880-wide viewBox
        // clipped silently. Every drawn node must now lie fully inside the
        // returned canvas bounds.
        let steps: Vec<Step> = (0..9).map(|i| step(&format!("Process step number {i}"))).collect();
        let (body, w, h) = render(&steps);
        for (x, y, rw, rh) in extract_rects(&body) {
            assert!(x + rw <= w + 0.5, "rect at x={x} w={rw} exceeds canvas width {w}");
            assert!(y + rh <= h + 0.5, "rect at y={y} h={rh} exceeds canvas height {h}");
        }
    }

    #[test]
    fn renders_caller_supplied_text_not_a_canned_default() {
        let steps = vec![step("Alpha"), step("Bravo"), step("Charlie")];
        let (body, _, _) = render(&steps);
        assert!(body.contains("Alpha"));
        assert!(body.contains("Bravo"));
        assert!(body.contains("Charlie"));
        assert!(!body.contains("Ingest Data"), "must not fall back to the retired canned pipeline");
    }

    fn extract_rects(svg: &str) -> Vec<(f32, f32, f32, f32)> {
        let mut out = Vec::new();
        for cap_start in svg.match_indices("<rect ") {
            let tag_end = svg[cap_start.0..].find('/').map(|i| cap_start.0 + i).unwrap_or(svg.len());
            let tag = &svg[cap_start.0..tag_end];
            let get = |attr: &str| -> f32 {
                tag.split(&format!(r#"{attr}=""#))
                    .nth(1)
                    .and_then(|rest| rest.split('"').next())
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.0)
            };
            out.push((get("x"), get("y"), get("width"), get("height")));
        }
        out
    }
}
