//! `generate_accessible_mermaid` payload builder.
//!
//! Mermaid is rendered **server-side** by the [Merman] Rust crate (a
//! browserless, parity-focused reimplementation of Mermaid.js) into a static
//! `<svg>` string, then wrapped in the same standalone HTML document as the
//! other viz tools and played in the sandboxed iframe. No JavaScript runtime
//! ships in the result (previously a ~2 MB vendored `mermaid.min.js` inlined
//! per result ran `mermaid.render()` at display time).
//!
//! We render through Merman's **`resvg_safe`** SVG pipeline rather than its
//! default parity pipeline. The parity output preserves Mermaid's native
//! `foreignObject` HTML labels; the resvg-safe pipeline replaces those with
//! plain `<text>/<rect>` SVG. That both (a) keeps the diagram free of
//! HTML-in-SVG under the iframe's `script-src 'unsafe-inline'` CSP, and (b)
//! produces a consistent, browser-agnostic SVG like every other viz tool (no
//! inline `background-color` / `max-width` that would fight the wrapper's
//! scaling and background).
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

use super::{success_payload, wrap_in_standalone_html};

const MAX_SOURCE_CHARS: usize = 12_000;

/// A cached Merman renderer, built once and reused across every Mermaid tool
/// call. Constructing a `HeadlessRenderer` (which materializes the full
/// Mermaid engine + render environment) on every call was wasteful for a
/// long-lived stdio server. `OnceLock` gives us a process-wide singleton; we
/// configure it once with the resvg-safe pipeline so every result comes back
/// through that pipeline.
static RENDERER: OnceLock<HeadlessRenderer> = OnceLock::new();

fn renderer() -> &'static HeadlessRenderer {
    RENDERER.get_or_init(|| {
        HeadlessRenderer::new().with_svg_pipeline(SvgPipeline::resvg_safe())
    })
}

/// Extracts the width component of a rendered SVG's `viewBox="min-x min-y w h"`.
/// Used to reject diagrams that exceed the readability budget (parity with the
/// other viz tools' `VIZ_TOO_WIDE` guard).
fn svg_viewbox_width(svg: &str) -> Option<f32> {
    let vb = svg.split("viewBox=\"").nth(1)?.split('"').next()?;
    let mut parts = vb.split_whitespace();
    let _ = parts.next()?; // min-x
    let _ = parts.next()?; // min-y
    parts.next()?.parse().ok()
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
    // than the chat iframe shrinks below legible size once scaled to fit.
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

    let body = format!(
        "<div style=\"padding:8px 0;\">{svg}</div>\
         <p class=\"sr-only\">{}</p>",
        super::escape::escape_text(description)
    );
    let standalone = wrap_in_standalone_html(title, &body);
    success_payload(title, &standalone, &[])
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
        let v: Value =
            serde_json::from_str(&generate_accessible_mermaid("this is not a diagram", "T", "D"))
                .unwrap();
        assert_eq!(v["status"], "error");
        assert_eq!(v["error_code"], "VIZ_MERMAID_RENDER_FAILED");
    }

    #[test]
    fn success_payload_embeds_a_static_svg() {
        let out = generate_accessible_mermaid(
            "flowchart TD\nA-->B",
            "Flow",
            "A goes to B",
        );
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["render_config"]["target"], "iframe");
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
        // Wrapped in our standalone document like the other viz tools.
        assert!(html.contains("<!DOCTYPE html>"));
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
        // Audit #132: the rendered SVG is interpolated verbatim into the
        // standalone wrapper, whose CSP allows inline scripts inside an
        // `allow-scripts` iframe — so a label that breaks out as live markup
        // would execute. The render pipeline must escape label text (or the
        // source must be rejected outright).
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
        assert_eq!(svg_viewbox_width(r#"<svg viewBox="0 0 85 174">"#), Some(85.0));
        assert_eq!(svg_viewbox_width(r#"<svg viewBox="10 -5 300 200">"#), Some(300.0));
        assert_eq!(svg_viewbox_width(r#"<svg>"#), None);
    }
}
