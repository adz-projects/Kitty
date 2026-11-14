//! `generate_accessible_mermaid` payload builder.
//!
//! Mermaid is rendered **server-side** by the [Merman] Rust crate (a
//! browserless, parity-focused reimplementation of Mermaid.js) into a static
//! `<svg>` string, then wrapped in the same `<figure>` as the other viz tools
//! and mounted inline by the host. No JavaScript runtime ships in the result.
//!
//! We render through Merman's **`resvg_safe`** SVG pipeline rather than its
//! default parity pipeline. The parity output preserves Mermaid's native
//! `foreignObject` HTML labels; the resvg-safe pipeline replaces those with
//! plain `<text>/<rect>` SVG, which keeps HTML-in-SVG out of a fragment the
//! host mounts in its own document.
//!
//! # Theme
//!
//! Mermaid bakes its palette into the SVG (a `<style>` block plus presentation
//! attributes), so it cannot be themed by class name the way this crate's own
//! output is. Instead it is rendered with Mermaid's `base` theme and a palette
//! of **sentinel colours** (`THEME`), each standing for one role — ink, paper,
//! rule, tint, accent — and every occurrence of a sentinel in the output is
//! then rewritten to the host's custom property for that role
//! (`var(--vz-ink)` ...). The result follows the app's theme, light or dark,
//! exactly like the other three tools. A colour Mermaid derives on its own
//! rather than taking from the palette stays as rendered; `THEME` sets every
//! variable the common diagram types read so that those are few.
//!
//! "Foolproof" here means *guaranteed degradation, never a blank frame*: the
//! server rejects empty/oversized sources up front, and any Merman parse,
//! layout, render, or missing-capability failure returns a visible error
//! envelope instead of silently dropping the diagram.
//!
//! [Merman]: https://github.com/Latias94/merman

use std::sync::OnceLock;

use crate::envelope::error_response;
use merman::svg::{HeadlessRenderer, SvgPipeline};
use merman::MermaidConfig;
use serde_json::json;

use super::{figure, success_payload};

const MAX_SOURCE_CHARS: usize = 12_000;

/// Sentinel colour -> the host custom property it stands for. The sentinels
/// are arbitrary but distinct, and unlikely to be produced by Mermaid's own
/// colour arithmetic.
const ROLES: [(&str, (u8, u8, u8), &str); 6] = [
    ("#010203", (1, 2, 3), "var(--vz-ink)"),
    ("#fefdfc", (254, 253, 252), "var(--vz-paper)"),
    ("#8b8a89", (139, 138, 137), "var(--vz-rule)"),
    ("#6b6a69", (107, 106, 105), "var(--vz-muted)"),
    ("#ecebea", (236, 235, 234), "var(--vz-tint)"),
    ("#d00201", (208, 2, 1), "var(--vz-accent)"),
];
const INK: &str = ROLES[0].0;
const PAPER: &str = ROLES[1].0;
const RULE: &str = ROLES[2].0;
const MUTED: &str = ROLES[3].0;
const TINT: &str = ROLES[4].0;
const ACCENT: &str = ROLES[5].0;

/// Mermaid `themeVariables` for the house style: paper-coloured shapes with
/// ink outlines and ink type, set in the same grotesque as the other figures.
fn theme() -> MermaidConfig {
    let mut vars = serde_json::Map::new();
    let mut set = |keys: &[&str], value: &str| {
        for k in keys {
            vars.insert((*k).to_string(), json!(value));
        }
    };
    set(
        &[
            "primaryTextColor",
            "secondaryTextColor",
            "tertiaryTextColor",
            "textColor",
            "titleColor",
            "lineColor",
            "primaryBorderColor",
            "secondaryBorderColor",
            "tertiaryBorderColor",
            "nodeBorder",
            "nodeTextColor",
            "clusterBorder",
            "defaultLinkColor",
            "arrowheadColor",
            "actorBorder",
            "actorTextColor",
            "actorLineColor",
            "signalColor",
            "signalTextColor",
            "labelBoxBorderColor",
            "labelTextColor",
            "loopTextColor",
            "noteBorderColor",
            "noteTextColor",
            "activationBorderColor",
            "sequenceNumberColor",
            "classText",
            "labelColor",
            "transitionColor",
            "transitionLabelColor",
            "stateLabelColor",
            "stateBorder",
            "specialStateColor",
            "innerEndBackground",
            "compositeBorder",
            "attributeBackgroundColorOdd",
            "relationColor",
            "pieTitleTextColor",
            "pieLegendTextColor",
            "pieOuterStrokeColor",
            "taskTextColor",
            "taskTextOutsideColor",
            "taskTextLightColor",
            "taskTextDarkColor",
            "taskTextClickableColor",
            "sectionBkgColor2",
            "taskBorderColor",
            "activeTaskBorderColor",
            "doneTaskBorderColor",
            "critBorderColor",
            "gridColor",
            "todayLineColor",
            "git0",
            "pie1",
            "pie5",
            "pie9",
            "errorBkgColor",
            "errorTextColor",
        ],
        INK,
    );
    set(
        &[
            "background",
            "primaryColor",
            "mainBkg",
            "secondBkg",
            "edgeLabelBackground",
            "actorBkg",
            "labelBoxBkgColor",
            "classBkgColor",
            "stateBkg",
            "compositeBackground",
            "compositeTitleBackground",
            "attributeBackgroundColorEven",
            "altBackground",
            "sectionBkgColor",
            "altSectionBkgColor",
            "taskBkgColor",
            "doneTaskBkgColor",
            "relationLabelBackground",
            "labelBackground",
            "pieSectionTextColor",
            "pieStrokeColor",
        ],
        PAPER,
    );
    set(
        &[
            "secondaryColor",
            "tertiaryColor",
            "clusterBkg",
            "noteBkgColor",
            "activationBkgColor",
            "activeTaskBkgColor",
            "excludeBkgColor",
            "git2",
        ],
        TINT,
    );
    // Pie slices carry their percentage in paper-coloured type, so every
    // slice colour is one that type reads on.
    set(&["pie4", "pie8", "pie12", "git3"], RULE);
    set(&["pie3", "pie7", "pie11", "git4"], MUTED);
    set(&["critBkgColor", "pie2", "pie6", "pie10", "git1"], ACCENT);
    vars.insert("pieOpacity".to_string(), json!("1"));
    vars.insert("pieStrokeWidth".to_string(), json!("1px"));
    vars.insert("pieOuterStrokeWidth".to_string(), json!("1px"));
    vars.insert(
        "fontFamily".to_string(),
        json!("Helvetica, Arial, sans-serif"),
    );
    vars.insert("fontSize".to_string(), json!("14px"));
    MermaidConfig::from_value(json!({
        "theme": "base",
        "fontFamily": "Helvetica, Arial, sans-serif",
        "themeVariables": vars,
    }))
}

/// Colours some Mermaid diagram types hard-code instead of reading from the
/// theme (sequence actors and notes, state notes, the black of markers and
/// rules, the half-opaque label backings), mapped to the role they play. Each
/// pattern includes its delimiter so a short hex can't match inside a longer
/// one.
const FIXED: [(&str, &str); 26] = [
    ("#000000", "var(--vz-ink)"),
    ("\"#000\"", "\"var(--vz-ink)\""),
    (":#000;", ":var(--vz-ink);"),
    (":#000}", ":var(--vz-ink)}"),
    ("\"black\"", "\"var(--vz-ink)\""),
    (":black;", ":var(--vz-ink);"),
    (":black}", ":var(--vz-ink)}"),
    ("\"#666\"", "\"var(--vz-ink)\""),
    (":#666;", ":var(--vz-ink);"),
    (":#666}", ":var(--vz-ink)}"),
    ("\"#999\"", "\"var(--vz-rule)\""),
    (":#999;", ":var(--vz-rule);"),
    (":#999}", ":var(--vz-rule)}"),
    ("rgba(185,185,185,1)", "var(--vz-rule)"),
    ("\"white\"", "\"var(--vz-paper)\""),
    (":white;", ":var(--vz-paper);"),
    (":white}", ":var(--vz-paper)}"),
    ("rgba(254, 253, 252, 0.5)", "var(--vz-paper)"),
    ("rgba(232, 232, 232, 0.5)", "var(--vz-paper)"),
    ("rgba(236, 235, 234, 0.5)", "var(--vz-tint)"),
    ("#eaeaea", "var(--vz-tint)"),
    ("#e0e0e0", "var(--vz-tint)"),
    ("#EDF2AE", "var(--vz-tint)"),
    ("#edf2ae", "var(--vz-tint)"),
    ("#fff4dd", "var(--vz-tint)"),
    ("#FFF4DD", "var(--vz-tint)"),
];

/// Rewrites every sentinel colour in a rendered SVG to the host custom
/// property it stands for, then the handful of colours in `FIXED`. Mermaid writes a colour as lower- or upper-case
/// hex or as `rgb(r, g, b)` depending on where it ends up, so all three
/// spellings are covered.
fn apply_roles(svg: &str) -> String {
    let mut out = svg.to_string();
    for (hex, (r, g, b), var) in ROLES {
        out = out.replace(hex, var);
        out = out.replace(&hex.to_uppercase(), var);
        out = out.replace(&format!("rgb({r}, {g}, {b})"), var);
        out = out.replace(&format!("rgb({r},{g},{b})"), var);
    }
    for (from, to) in FIXED {
        out = out.replace(from, to);
    }
    out
}

/// Drops the `background-color` Mermaid sets inline on the root `<svg>`, so
/// the figure sits on the chat's own surface.
fn strip_root_background(svg: &str) -> String {
    let Some(tag_end) = svg.find('>') else {
        return svg.to_string();
    };
    let (root, rest) = svg.split_at(tag_end);
    let Some(start) = root.find("background-color:") else {
        return svg.to_string();
    };
    let after = &root[start..];
    let len = after
        .find([';', '"'])
        .map(|i| {
            if after[i..].starts_with(';') {
                i + 1
            } else {
                i
            }
        })
        .unwrap_or(after.len());
    format!("{}{}{rest}", &root[..start], &root[start + len..])
}

/// A cached Merman renderer, built once and reused across every Mermaid tool
/// call. Constructing a `HeadlessRenderer` (which materializes the full
/// Mermaid engine + render environment) on every call was wasteful for a
/// long-lived stdio server. `OnceLock` gives us a process-wide singleton,
/// configured once with the house theme and the resvg-safe pipeline.
static RENDERER: OnceLock<HeadlessRenderer> = OnceLock::new();

fn renderer() -> &'static HeadlessRenderer {
    RENDERER.get_or_init(|| {
        HeadlessRenderer::new()
            .with_site_config(theme())
            .with_svg_pipeline(SvgPipeline::resvg_safe())
    })
}

/// Extracts the width component of a rendered SVG's `viewBox="min-x min-y w h"`.
/// Used to reject diagrams that exceed the readability budget (parity with the
/// other viz tools' `VIZ_TOO_WIDE` guard).
fn svg_viewbox_width(svg: &str) -> Option<f32> {
    svg_viewbox_dims(svg).map(|(w, _)| w)
}

/// Extracts `(width, height)` from a rendered SVG's `viewBox="min-x min-y w h"`.
fn svg_viewbox_dims(svg: &str) -> Option<(f32, f32)> {
    let vb = svg.split("viewBox=\"").nth(1)?.split('"').next()?;
    let mut parts = vb.split_whitespace();
    let _ = parts.next()?; // min-x
    let _ = parts.next()?; // min-y
    let w: f32 = parts.next()?.parse().ok()?;
    let h: f32 = parts.next()?.parse().ok()?;
    Some((w, h))
}

pub fn generate_accessible_mermaid(mermaid: &str, title: &str, description: &str) -> String {
    if mermaid.trim().is_empty() {
        return error_response(
            "VIZ_EMPTY_MERMAID",
            "No Mermaid source was provided.",
            None,
            Some("Provide a non-empty Mermaid `mermaid` string, e.g. \"flowchart TD\\nA-->B\"."),
        );
    }
    let source_chars = mermaid.chars().count();
    if source_chars > MAX_SOURCE_CHARS {
        return error_response(
            "VIZ_MERMAID_TOO_LARGE",
            &format!(
                "The Mermaid source is {source_chars} characters; at most {MAX_SOURCE_CHARS} are allowed."
            ),
            None,
            Some("Split the diagram into smaller pieces, or simplify it."),
        );
    }

    let svg = match renderer().render_svg_with_pipeline_sync(mermaid, &SvgPipeline::resvg_safe()) {
        Ok(Some(svg)) => svg,
        Ok(None) => {
            return error_response(
                "VIZ_MERMAID_RENDER_FAILED",
                "No Mermaid diagram was detected in the source.",
                None,
                Some("Check that the `mermaid` source starts with a recognized diagram type."),
            );
        }
        Err(e) => {
            return error_response(
                "VIZ_MERMAID_RENDER_FAILED",
                &format!("Mermaid could not be rendered: {e}"),
                None,
                Some("Check the `mermaid` source for parsing errors, or simplify the diagram."),
            );
        }
    };

    // Same readability guard as `generate_accessible_svg`: a diagram wider
    // than the chat column shrinks below legible size once scaled to fit.
    // Mermaid diagrams that can't be sensibly compressed use the wide budget.
    if let Some(w) = svg_viewbox_width(&svg) {
        let budget = super::layout::MAX_CONTENT_W_WIDE;
        if w > budget + super::layout::WIDTH_SLACK {
            return error_response(
                "VIZ_TOO_WIDE",
                &format!(
                    "This diagram is {w:.0}px wide, wider than the {budget:.0}px readability budget, so it would render illegibly small in the chat."
                ),
                None,
                Some("Reduce the number of steps or nodes, or split the diagram into smaller ones."),
            );
        }
    }

    let svg = apply_roles(&strip_root_background(&svg));
    let body = format!(
        r#"{svg}<p class="sr-only">{}</p>"#,
        super::escape::escape_text(description)
    );
    success_payload("mermaid", title, &figure("mermaid", title, &body), &[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn empty_source_is_rejected() {
        let v: Value = serde_json::from_str(&generate_accessible_mermaid("   ", "T", "D")).unwrap();
        assert_eq!(v["status"], "error");
        assert_eq!(v["error_code"], "VIZ_EMPTY_MERMAID");
        assert!(v["hint"].is_string());
    }

    #[test]
    fn oversized_source_is_rejected() {
        let big = "flowchart TD\n".to_string() + &"x".repeat(MAX_SOURCE_CHARS + 10);
        let v: Value = serde_json::from_str(&generate_accessible_mermaid(&big, "T", "D")).unwrap();
        assert_eq!(v["error_code"], "VIZ_MERMAID_TOO_LARGE");
    }

    #[test]
    fn invalid_source_is_rejected_server_side() {
        // Prose/empty-with-no-diagram must produce an error envelope, not a
        // (previously JS-time) blank frame.
        let v: Value = serde_json::from_str(&generate_accessible_mermaid(
            "this is not a diagram",
            "T",
            "D",
        ))
        .unwrap();
        assert_eq!(v["status"], "error");
        assert_eq!(v["error_code"], "VIZ_MERMAID_RENDER_FAILED");
    }

    #[test]
    fn success_payload_embeds_a_static_svg() {
        let out = generate_accessible_mermaid("flowchart TD\nA-->B", "Flow", "A goes to B");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["render_config"]["target"], "inline");
        assert_eq!(v["render_config"]["kind"], "mermaid");
        let html = v["html_payload"].as_str().unwrap();
        // Server-rendered: the SVG is baked in, no mermaid JS runtime inline.
        assert!(html.contains("<svg"), "must embed a rendered <svg>");
        assert!(
            !html.contains("mermaid.initialize"),
            "must not inline the mermaid JS runtime"
        );
        // resvg-safe pipeline: no HTML-in-SVG `foreignObject` labels, just
        // plain SVG text, and no inline white background box on the root.
        assert!(
            !html.contains("foreignObject"),
            "resvg-safe pipeline must not emit foreignObject HTML labels"
        );
        // A fragment in the shared figure, like the other viz tools: no
        // document wrapper and no script.
        assert!(html.starts_with(r#"<figure class="viz viz-mermaid">"#));
        assert!(!html.contains("<script"));
        assert!(!html.contains("<!DOCTYPE"));
    }

    #[test]
    fn the_palette_is_rewritten_to_host_custom_properties() {
        for source in [
            "flowchart TD\nA[Start] --> B{Valid?}\nB -->|yes| C[Save]\nB -->|no| D[Reject]",
            "sequenceDiagram\nAlice->>Bob: Hello\nNote over Alice,Bob: A note",
            "pie title Pets\n\"Dogs\" : 386\n\"Cats\" : 85",
        ] {
            let out = generate_accessible_mermaid(source, "T", "D");
            let v: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(v["status"], "success", "{out}");
            let html = v["html_payload"].as_str().unwrap();
            assert!(html.contains("var(--vz-ink)"), "ink role missing: {source}");
            for (hex, (r, g, b), _) in ROLES {
                assert!(
                    !html.to_lowercase().contains(hex),
                    "sentinel {hex} survived in {source}"
                );
                assert!(!html.contains(&format!("rgb({r}, {g}, {b})")));
            }
            let root = &html[html.find("<svg").unwrap()..];
            let root = &root[..root.find('>').unwrap()];
            assert!(
                !root.contains("background-color"),
                "root background must be dropped: {root}"
            );
            for baked in ["#eaeaea", "\"#666\"", "\"black\"", "#000000"] {
                assert!(!html.contains(baked), "{baked} left baked in for {source}");
            }
        }
    }

    #[test]
    fn root_background_is_stripped_whatever_follows_it() {
        assert_eq!(
            strip_root_background(
                r#"<svg style="max-width:10px;background-color:white"><g/></svg>"#
            ),
            r#"<svg style="max-width:10px;"><g/></svg>"#
        );
        assert_eq!(
            strip_root_background(
                r#"<svg style="background-color:#fff;max-width:10px"><g/></svg>"#
            ),
            r#"<svg style="max-width:10px"><g/></svg>"#
        );
        assert_eq!(strip_root_background("<svg><g/></svg>"), "<svg><g/></svg>");
    }

    #[test]
    fn description_is_emitted_as_screen_reader_text() {
        let out = generate_accessible_mermaid("pie\n\"A\":1", "Pie", "A single slice.");
        let v: Value = serde_json::from_str(&out).unwrap();
        let html = v["html_payload"].as_str().unwrap();
        assert!(html.contains("A single slice."));
    }

    #[test]
    fn hostile_labels_cannot_inject_markup() {
        // Audit #132: the rendered SVG is handed to the host verbatim, so a
        // label that breaks out as live markup would reach its sanitizer as
        // markup. The render pipeline must escape label text (or the source
        // must be rejected outright).
        let mut rendered = 0;
        for source in [
            "flowchart TD\nA[\"<script>alert(1)</script>\"]-->B",
            "flowchart TD\nA[\"<img src=x onerror=alert(1)>\"]-->B",
            "flowchart TD\nA[\"</title><script>alert(1)</script>\"]-->B",
        ] {
            let out = generate_accessible_mermaid(source, "T", "D");
            let v: Value = serde_json::from_str(&out).unwrap();
            if v["status"] == "error" {
                continue; // rejected outright is also safe
            }
            rendered += 1;
            let html = v["html_payload"].as_str().unwrap();
            assert!(
                !html.contains("<script>alert(1)</script>"),
                "label script tag survived verbatim: {html}"
            );
            assert!(!html.contains("onerror"), "event handler survived: {html}");
            assert!(!html.contains("<img"), "raw img tag survived: {html}");
        }
        // The test is meaningless if every source happened to fail rendering
        // — at least one hostile label must reach the SVG pipeline.
        assert!(rendered > 0, "no hostile label was actually rendered");
    }

    #[test]
    fn viewbox_width_is_parsed() {
        assert_eq!(
            svg_viewbox_width(r#"<svg viewBox="0 0 85 174">"#),
            Some(85.0)
        );
        assert_eq!(
            svg_viewbox_width(r#"<svg viewBox="10 -5 300 200">"#),
            Some(300.0)
        );
        assert_eq!(svg_viewbox_width(r#"<svg>"#), None);
    }
}
