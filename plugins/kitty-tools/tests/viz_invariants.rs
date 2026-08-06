//! Parse-based invariant tests for the diagram layouts: node boxes never
//! overlap, swimlane nodes never overdraw their lane headers, long (skip-level)
//! edges are rejected, and every rendered diagram stays within the readability
//! width budget. These guard the "foolproof" guarantees added with the
//! no-overlap/readability work — the layouts could regress silently without
//! them (the old test suite only checked rects stayed inside the canvas).

use kitty_tools::tools::viz::model::{DiagramType, Step};
use kitty_tools::tools::viz::generate_accessible_svg;
use serde_json::Value;

#[derive(Debug)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    class: String,
}

fn rect_attrs(tag: &str, name: &str) -> f32 {
    tag.split(&format!(r#"{name}=""#))
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

/// Parses every `<rect>` (and its class) out of a rendered SVG body.
fn parse_rects(svg: &str) -> Vec<Rect> {
    let mut out = Vec::new();
    for cap_start in svg.match_indices("<rect ") {
        let tag_end = svg[cap_start.0..].find("/>").map(|i| cap_start.0 + i).unwrap_or(svg.len());
        let tag = &svg[cap_start.0..tag_end];
        out.push(Rect {
            x: rect_attrs(tag, "x"),
            y: rect_attrs(tag, "y"),
            w: rect_attrs(tag, "width"),
            h: rect_attrs(tag, "height"),
            class: rect_attr_str(tag, "class"),
        });
    }
    out
}

fn rect_attr_str(tag: &str, name: &str) -> String {
    tag.split(&format!(r#"{name}=""#))
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or("")
        .to_string()
}

fn overlaps(a: &Rect, b: &Rect) -> bool {
    a.x < b.x + b.w && a.x + a.w > b.x && a.y < b.y + b.h && a.y + a.h > b.y
}

fn html_of(payload: &str) -> String {
    let v: Value = serde_json::from_str(payload).unwrap();
    assert_eq!(v["status"], "success", "expected success envelope, got: {payload}");
    v["html_payload"].as_str().unwrap().to_string()
}

fn svg_of(html: &str) -> String {
    let start = html.find("<svg").unwrap();
    let end = html.find("</svg>").map(|i| i + "</svg>".len()).unwrap_or(html.len());
    html[start..end].to_string()
}

fn svg_width(html: &str) -> f32 {
    let svg = svg_of(html);
    let vb = svg.split("viewBox=\"").nth(1).and_then(|r| r.split('"').next()).expect("no viewBox");
    let mut parts = vb.split_whitespace();
    let _ = parts.next(); // min-x
    let _ = parts.next(); // min-y
    parts.next().and_then(|v| v.parse().ok()).expect("viewBox width")
}

fn step(id: &str, text: &str, next: &[&str]) -> Step {
    Step { id: Some(id.to_string()), text: text.to_string(), next: next.iter().map(|s| s.to_string()).collect(), ..Default::default() }
}

#[test]
fn node_boxes_never_overlap_in_any_diagram_type() {
    let cases: Vec<(DiagramType, Vec<Step>)> = vec![
        (
            DiagramType::SingleLane,
            (0..6).map(|i| step(&format!("s{i}"), &format!("Process step number {i} with a label"), &[])).collect(),
        ),
        (
            DiagramType::Flowchart,
            vec![
                step("a", "Start", &["b", "c"]),
                step("b", "Left branch", &["d"]),
                step("c", "Right branch with a longer label", &["d"]),
                step("d", "Merge", &["e", "f"]),
                step("e", "Out one", &[]),
                step("f", "Out two", &[]),
            ],
        ),
        (
            DiagramType::Tree,
            vec![
                step("ceo", "CEO", &["eng", "sales", "ops"]),
                step("eng", "VP Engineering", &["fe", "be"]),
                step("fe", "Frontend", &[]),
                step("be", "Backend", &[]),
                step("sales", "VP Sales", &[]),
                step("ops", "VP Operations", &[]),
            ],
        ),
        (
            DiagramType::Swimlane,
            (0..8)
                .map(|i| Step { text: format!("Step {i}"), lane: Some(if i % 2 == 0 { "Customer" } else { "Backend" }.to_string()), ..Default::default() })
                .collect(),
        ),
    ];

    for (dtype, steps) in cases {
        let out = generate_accessible_svg(dtype, "T", "D", steps);
        let html = html_of(&out);
        let rects: Vec<Rect> = parse_rects(&html).into_iter().filter(|r| r.class.contains("node-box")).collect();
        assert!(rects.len() >= 2, "{dtype:?}: expected multiple node boxes");
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                assert!(!overlaps(a, b), "{dtype:?}: node rects overlap: {a:?} vs {b:?}");
            }
        }
    }
}

#[test]
fn swimlane_nodes_stay_below_their_lane_headers() {
    // A tall (3-line) node used to be vertically centered against the band top
    // and overdraw the "CUSTOMER" header at band_top+22. Headers are now
    // reserved a LANE_HEADER_H strip, so every node top must clear the header
    // baseline by a margin.
    let steps: Vec<Step> = (0..6)
        .map(|i| Step { text: format!("Step {i} with a deliberately long description that wraps to three lines for sure"), lane: Some("Customer".to_string()), ..Default::default() })
        .collect();
    let out = generate_accessible_svg(DiagramType::Swimlane, "T", "D", steps);
    let html = html_of(&out);

    let mut header_ys: Vec<f32> = Vec::new();
    let mut scan_from = 0;
    while let Some(rel) = html[scan_from..].find("<text ") {
        let start = scan_from + rel;
        let tag_end = html[start..].find('>').map(|i| start + i).unwrap_or(html.len());
        let tag = &html[start..tag_end];
        if tag.contains("lane-header") {
            header_ys.push(rect_attrs(tag, "y"));
        }
        scan_from = start + 4;
    }
    assert!(!header_ys.is_empty(), "no lane-header text found");

    let nodes = parse_rects(&html).into_iter().filter(|r| r.class.contains("node-box")).collect::<Vec<_>>();
    assert!(!nodes.is_empty(), "no node-box rects found in: {html}");
    let min_node_top = nodes.iter().map(|n| n.y).fold(f32::MAX, f32::min);
    let max_header_base = header_ys.iter().cloned().fold(f32::MIN, f32::max);
    assert!(
        min_node_top >= max_header_base + 3.0,
        "node top ({min_node_top}) must clear the lane-header baseline ({max_header_base}) by a margin"
    );
}

#[test]
fn skip_level_edges_are_rejected_with_a_hint() {
    // a -> b and a -> c, with b -> c: c is two layers below a (a->b->c), so the
    // direct a->c edge jumps a layer and must be rejected.
    let steps = vec![
        step("a", "Start", &["b", "c"]),
        step("b", "Intermediate", &["c"]),
        step("c", "End", &[]),
    ];
    let out = generate_accessible_svg(DiagramType::Flowchart, "T", "D", steps);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["status"], "error", "expected error envelope, got: {out}");
    assert_eq!(v["error_code"], "VIZ_LONG_EDGE", "got error envelope: {out}");
    assert!(v["hint"].as_str().unwrap().contains("intermediate"));

    // Same guarantee for trees, via the single-parent rule: in a strict tree a
    // skip edge always gives the grandchild a second parent, so the multi-parent
    // check rejects it first (equally "foolproof", different code path).
    let steps = vec![
        step("a", "Root", &["c"]),
        step("b", "Middle", &["c"]),
        step("c", "Leaf", &[]),
    ];
    let out = generate_accessible_svg(DiagramType::Tree, "T", "D", steps);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["error_code"], "VIZ_BAD_EDGE_REF", "got: {out}");
}

#[test]
fn every_diagram_stays_within_the_readability_budget() {
    // A dense single-layer flowchart (the worst case for width) must be
    // compressed inside the budget rather than rendered illegibly small.
    let steps: Vec<Step> = (0..8)
        .map(|i| step(&format!("n{i}"), &format!("Node number {i} with a long descriptive label"), &[]))
        .collect();
    let out = generate_accessible_svg(DiagramType::Flowchart, "T", "D", steps);
    let v: Value = serde_json::from_str(&out).unwrap();
    if v["status"] == "error" {
        assert_eq!(v["error_code"], "VIZ_TOO_WIDE", "unexpected error: {out}");
    } else {
        let html = v["html_payload"].as_str().unwrap();
        assert!(svg_width(html) <= 1100.0 + 60.0, "flowchart exceeds readability budget: {}px", svg_width(html));
    }
}

#[test]
fn long_flowchart_labels_stay_inside_their_node_boxes() {
    // Regression: the text primitives used to track only their anchor point in
    // `Bounds`, so a label wider (or taller) than its node box was invisible to
    // the layout's width/height budget — the text could spill onto the next
    // node or a connecting vector. The renderer now folds each `<text>`'s
    // measured extent into `Bounds`; assert the rendered label is contained
    // within its own node-box rect even for the widest labels we allow.
    let steps: Vec<Step> = vec![
        step("a", "An extremely wide node label that must never spill", &["b"]),
        step("b", "Another very long label that also must not overflow", &["c"]),
        step("c", "A third long descriptive label for the chain", &["d"]),
        step("d", "Final very long label one", &[]),
    ];
    let out = generate_accessible_svg(DiagramType::Flowchart, "T", "D", steps);
    let html = html_of(&out);

    // Reuse the display-dpi-independent estimator the layout itself uses.
    const NODE_FONT_PX: f32 = 12.5;
    let mut scan_from = 0;
    let mut text_count = 0;
    while let Some(rel) = html[scan_from..].find("<text ") {
        let start = scan_from + rel;
        // End of the `<text ...>` OPENING tag (class/x/y live here).
        let open_end = html[start..].find('>').map(|i| start + i).unwrap_or(html.len());
        let opening = &html[start..open_end];
        // The full element, ending at the matching `</text>`.
        let close_at = html[start..].find("</text>").map(|i| start + i).unwrap_or(html.len());
        let element = &html[start..close_at];
        if opening.contains("node-text") && !opening.contains("title-text") {
            let x = rect_attrs(opening, "x");
            let y = rect_attrs(opening, "y");
            // Extract every `<tspan ...>TEXT</tspan>` label; pick the widest.
            let mut widest = 0.0_f32;
            let mut tspan_scan = 0;
            while let Some(trel) = element[tspan_scan..].find("<tspan ") {
                let tstart = tspan_scan + trel;
                // The tspan's opening tag ends at its first `>`; the label text
                // is everything until the next `<`.
                let open_end = element[tstart..].find('>').map(|i| tstart + i).unwrap_or(element.len());
                let body = &element[open_end + 1..];
                let text = body.split('<').next().unwrap_or("").to_string();
                widest = widest.max(kitty_tools::tools::viz::text::measure_px(
                    &text
                        .replace("&lt;", "<")
                        .replace("&gt;", ">")
                        .replace("&amp;", "&")
                        .replace("&quot;", "\"")
                        .replace("&apos;", "'"),
                    NODE_FONT_PX,
                ));
                tspan_scan = open_end + 1;
            }
            assert!(
                widest > 0.0,
                "expected at least one tspan with text in node-text element: {element}"
            );
            let w = widest;
            text_count += 1;
            // Find the containing node-box that surrounds this text anchor.
            let box_rect = parse_rects(&html)
                .into_iter()
                .find(|r| r.class.contains("node-box") && x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h);
            assert!(
                box_rect.is_some(),
                "node-text at ({x:.1},{y:.1}) is not inside any node-box"
            );
            let b = box_rect.unwrap();
            assert!(
                x - w / 2.0 >= b.x - 0.5 && x + w / 2.0 <= b.x + b.w + 0.5,
                "text (half-width {:.1}) at x={x:.1} overflows its box [{:.1}, {:.1}]",
                w / 2.0,
                b.x,
                b.x + b.w
            );
        }
        scan_from = start + 4;
    }
    assert!(text_count > 0, "no node-text found to check");
}
