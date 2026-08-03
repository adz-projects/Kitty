//! `journey_map` — stages of a user experience, with optional feelings and
//! friction points. The crate used to hard-code a fixed 4-stage SaaS
//! onboarding funnel (including a literal hand-drawn sentiment curve)
//! regardless of input; every stage/action/sentiment/pain value here comes
//! from the caller.
//!
//! Bands are composed, not fixed: the USER ACTION row only appears if some
//! step has a `subtitle`, SENTIMENT only if some step has a `sentiment`, PAIN
//! POINTS only if some step has `pain`. A stage-only call renders just the
//! stage header row.

use crate::tools::viz::layout::NODE_FONT_PX;
use crate::tools::viz::model::Step;
use crate::tools::viz::render::svg::{SvgCanvas, CANVAS_MARGIN, TITLE_BAND};
use crate::tools::viz::text;

const LEFT_GUTTER: f32 = 100.0;
const MIN_COL_W: f32 = 160.0;
const STAGE_BAND_H: f32 = 50.0;
const ACTION_BAND_H: f32 = 70.0;
const SENTIMENT_BAND_H: f32 = 130.0;
const PAIN_BAND_H: f32 = 80.0;

pub fn render(steps: &[Step]) -> (String, f32, f32) {
    let n = steps.len();
    let col_w = steps.iter().map(|s| text::measure_px(&s.text, NODE_FONT_PX) + 24.0).fold(MIN_COL_W, f32::max);

    let has_action = steps.iter().any(|s| s.subtitle.as_deref().is_some_and(|t| !t.trim().is_empty()));
    let has_sentiment = steps.iter().any(|s| s.sentiment.is_some());
    let has_pain = steps.iter().any(|s| s.pain.as_deref().is_some_and(|t| !t.trim().is_empty()));

    let stage_top = TITLE_BAND;
    let action_top = stage_top + STAGE_BAND_H;
    let sentiment_top = action_top + if has_action { ACTION_BAND_H } else { 0.0 };
    let pain_top = sentiment_top + if has_sentiment { SENTIMENT_BAND_H } else { 0.0 };
    let content_bottom = pain_top + if has_pain { PAIN_BAND_H } else { 0.0 };

    let mut canvas = SvgCanvas::new();
    let total_w = LEFT_GUTTER + col_w * n as f32;

    for i in 0..n {
        if i % 2 == 1 {
            canvas.rect(LEFT_GUTTER + i as f32 * col_w, stage_top, col_w, content_bottom - stage_top, "journey-col-shade");
        }
    }

    canvas.line(0.0, stage_top + STAGE_BAND_H - 2.0, total_w, stage_top + STAGE_BAND_H - 2.0, "lane-divider");

    for (i, step) in steps.iter().enumerate() {
        let cx = LEFT_GUTTER + (i as f32 + 0.5) * col_w;
        canvas.text_line(cx, stage_top + 30.0, "node-text", &format!("{}. {}", i + 1, step.text));
    }

    if has_action {
        canvas.text_line(8.0, action_top + 40.0, "lane-header", "USER ACTION");
        for (i, step) in steps.iter().enumerate() {
            if let Some(subtitle) = step.subtitle.as_deref().filter(|t| !t.trim().is_empty()) {
                let cx = LEFT_GUTTER + (i as f32 + 0.5) * col_w;
                let lines = text::wrap(subtitle, col_w - 20.0, NODE_FONT_PX, 2);
                canvas.text_lines(cx, action_top + ACTION_BAND_H / 2.0, "node-text", &lines, 15.0);
            }
        }
    }

    if has_sentiment {
        canvas.text_line(8.0, sentiment_top + SENTIMENT_BAND_H / 2.0, "lane-header", "SENTIMENT");
        let points: Vec<(f32, f32)> = steps
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                s.sentiment.map(|sentiment| {
                    let cx = LEFT_GUTTER + (i as f32 + 0.5) * col_w;
                    let clamped = sentiment.clamp(-2, 2) as f32;
                    let cy = sentiment_top + SENTIMENT_BAND_H / 2.0 - clamped / 2.0 * (SENTIMENT_BAND_H / 2.0 - 15.0);
                    (cx, cy)
                })
            })
            .collect();

        if points.len() >= 2 {
            let mut d = format!("M {:.1},{:.1}", points[0].0, points[0].1);
            for w in points.windows(2) {
                let (x0, y0) = w[0];
                let (x1, y1) = w[1];
                let c1x = x0 + (x1 - x0) / 3.0;
                let c2x = x1 - (x1 - x0) / 3.0;
                d.push_str(&format!(" C {c1x:.1},{y0:.1} {c2x:.1},{y1:.1} {x1:.1},{y1:.1}"));
            }
            let min_x = points.iter().map(|p| p.0).fold(f32::MAX, f32::min);
            let max_x = points.iter().map(|p| p.0).fold(f32::MIN, f32::max);
            let min_y = points.iter().map(|p| p.1).fold(f32::MAX, f32::min);
            let max_y = points.iter().map(|p| p.1).fold(f32::MIN, f32::max);
            canvas.path(&d, "curve-line", (min_x, min_y, max_x - min_x, max_y - min_y));
        }
        for &(cx, cy) in &points {
            canvas.circle(cx, cy, 6.0, "curve-dot");
        }
    }

    if has_pain {
        canvas.text_line(8.0, pain_top + PAIN_BAND_H / 2.0, "lane-header", "PAIN POINTS");
        for (i, step) in steps.iter().enumerate() {
            if let Some(pain) = step.pain.as_deref().filter(|t| !t.trim().is_empty()) {
                let card_w = (col_w - 20.0).max(80.0);
                let card_h = 40.0;
                let cx = LEFT_GUTTER + (i as f32 + 0.5) * col_w;
                let card_x = cx - card_w / 2.0;
                let card_y = pain_top + (PAIN_BAND_H - card_h) / 2.0;
                canvas.rect(card_x, card_y, card_w, card_h, "pain-card");
                let lines = text::wrap(pain, card_w - 16.0, 10.5, 2);
                canvas.text_lines(cx, card_y + card_h / 2.0, "pain-text", &lines, 13.0);
            }
        }
    }

    canvas.reserve(0.0, 0.0, total_w, content_bottom);

    let (body, bounds) = canvas.into_parts();
    (body, bounds.width() + CANVAS_MARGIN, bounds.height() + CANVAS_MARGIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage(text: &str) -> Step {
        Step { text: text.to_string(), ..Default::default() }
    }

    #[test]
    fn renders_caller_supplied_stages_not_the_retired_saas_clipart() {
        let steps = vec![stage("Discovers product"), stage("Trials it"), stage("Buys plan")];
        let (body, _, _) = render(&steps);
        assert!(body.contains("Discovers product"));
        assert!(body.contains("Trials it"));
        assert!(body.contains("Buys plan"));
        assert!(!body.contains("Reads Overview"), "must not fall back to the retired SaaS-onboarding clipart");
        assert!(!body.contains("Fills Auth Form"), "must not fall back to the retired SaaS-onboarding clipart");
    }

    #[test]
    fn sentiment_band_only_appears_when_data_is_present() {
        let no_sentiment = vec![stage("A"), stage("B")];
        let (body_without, _, _) = render(&no_sentiment);
        assert!(!body_without.contains("SENTIMENT"));

        let with_sentiment = vec![
            Step { text: "A".to_string(), sentiment: Some(2), ..Default::default() },
            Step { text: "B".to_string(), sentiment: Some(-1), ..Default::default() },
        ];
        let (body_with, _, _) = render(&with_sentiment);
        assert!(body_with.contains("SENTIMENT"));
        assert!(body_with.contains("curve-dot"));
    }

    #[test]
    fn pain_band_only_appears_when_data_is_present() {
        let steps = vec![Step { text: "A".to_string(), pain: Some("Too many fields".to_string()), ..Default::default() }, stage("B")];
        let (body, _, _) = render(&steps);
        assert!(body.contains("PAIN POINTS"));
        assert!(body.contains("Too many fields"));
    }

    #[test]
    fn twelve_stages_stay_within_canvas_bounds() {
        let steps: Vec<Step> = (0..12).map(|i| stage(&format!("Stage {i}"))).collect();
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
