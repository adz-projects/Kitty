//! Shared SVG emission primitives. All user-supplied text passes through
//! `escape::escape_text`/`escape_attr` here, and nowhere else — layout modules
//! build up an `SvgCanvas` with these primitives and never `format!` raw user
//! text into markup themselves. This is what replaced the old crate-wide "no
//! escaping" policy: the guarantee lives in the API surface, not in caller
//! discipline.

use crate::tools::viz::escape::{escape_attr, escape_text};

/// Space reserved on the right/bottom of every diagram beyond the furthest
/// content actually drawn.
pub const CANVAS_MARGIN: f32 = 20.0;
/// Vertical space reserved at the top of every diagram for the title text that
/// `document()` draws; layout modules place their own content at or below this
/// y-coordinate.
pub const TITLE_BAND: f32 = 55.0;

#[derive(Debug, Default, Clone, Copy)]
pub struct Bounds {
    max_x: f32,
    max_y: f32,
}

impl Bounds {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn include_point(&mut self, x: f32, y: f32) {
        self.max_x = self.max_x.max(x);
        self.max_y = self.max_y.max(y);
    }

    pub fn include_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.include_point(x + w, y + h);
    }

    pub fn merge(&mut self, other: Bounds) {
        self.max_x = self.max_x.max(other.max_x);
        self.max_y = self.max_y.max(other.max_y);
    }

    pub fn width(&self) -> f32 {
        self.max_x
    }

    pub fn height(&self) -> f32 {
        self.max_y
    }
}

/// Accumulates an SVG body fragment and the bounding box of everything placed
/// into it. Every primitive here escapes user text and updates `bounds`
/// together, in the same call, so the two can never drift apart the way a
/// hand-written `format!` alongside a separately tracked bound could.
#[derive(Debug, Default)]
pub struct SvgCanvas {
    body: String,
    bounds: Bounds,
}

impl SvgCanvas {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bounds(&self) -> Bounds {
        self.bounds
    }

    /// Extends the tracked bounds without drawing anything — used when a
    /// layout needs to reserve space (e.g. a lane gutter) that isn't itself a
    /// single primitive call.
    pub fn reserve(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.bounds.include_rect(x, y, w, h);
    }

    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, class: &str) {
        self.bounds.include_rect(x, y, w, h);
        self.body
            .push_str(&format!(r#"<rect x="{x:.1}" y="{y:.1}" width="{w:.1}" height="{h:.1}" class="{class}"/>"#));
        self.body.push('\n');
    }

    pub fn text_line(&mut self, x: f32, y: f32, class: &str, content: &str) {
        self.bounds.include_point(x, y);
        self.body
            .push_str(&format!(r#"<text x="{x:.1}" y="{y:.1}" class="{class}">{}</text>"#, escape_text(content)));
        self.body.push('\n');
    }

    /// `text_line` plus a hard width backstop: when `content` is estimated
    /// wider than `max_width`, the browser is told to squeeze the glyphs to
    /// exactly `max_width` (`textLength` + `lengthAdjust="spacingAndGlyphs"`),
    /// so the label physically cannot overflow its container no matter how the
    /// user's `system-ui` font differs from the crate's metric table. When the
    /// text already fits (measurement over-estimates by design), no attribute
    /// is emitted and the text renders undistorted.
    pub fn text_line_fit(&mut self, x: f32, y: f32, class: &str, content: &str, font_size: f32, max_width: f32) {
        self.bounds.include_point(x, y);
        let text = escape_text(content);
        let mut tspans = format!(r#"<tspan x="{x:.1}">{text}</tspan>"#);
        if max_width > 1.0 && crate::tools::viz::text::measure_px(content, font_size) > max_width {
            tspans = format!(r#"<tspan x="{x:.1}" textLength="{max_width:.1}" lengthAdjust="spacingAndGlyphs">{text}</tspan>"#);
        }
        self.body
            .push_str(&format!(r#"<text x="{x:.1}" y="{y:.1}" class="{class}">{tspans}</text>"#));
        self.body.push('\n');
    }

    /// Multi-line label centered on `(x, y)`, relying on the `.node-text`
    /// house style's `dominant-baseline: middle`: the first tspan is offset up
    /// by half the block height and each subsequent one steps down by
    /// `line_h`, so the whole block ends up vertically centered on `y`.
    pub fn text_lines(&mut self, x: f32, y: f32, class: &str, lines: &[String], line_h: f32) {
        if lines.is_empty() {
            return;
        }
        self.bounds.include_point(x, y);
        let n = lines.len() as f32;
        let mut tspans = String::new();
        for (i, line) in lines.iter().enumerate() {
            let dy = if i == 0 { -(n - 1.0) * line_h / 2.0 } else { line_h };
            tspans.push_str(&format!(r#"<tspan x="{x:.1}" dy="{dy:.1}">{}</tspan>"#, escape_text(line)));
        }
        self.body.push_str(&format!(r#"<text x="{x:.1}" y="{y:.1}" class="{class}">{tspans}</text>"#));
        self.body.push('\n');
    }

    /// Multi-line label with the same hard width backstop as `text_line_fit`:
    /// each line whose *estimated* width exceeds `max_width` is handed to the
    /// browser with `textLength` + `lengthAdjust="spacingAndGlyphs"` so it can
    /// never paint wider than the container. Lines that already fit are
    /// untouched (no glyph distortion).
    #[allow(clippy::too_many_arguments)]
    pub fn text_lines_fit(&mut self, x: f32, y: f32, class: &str, lines: &[String], line_h: f32, font_size: f32, max_width: f32) {
        if lines.is_empty() {
            return;
        }
        self.bounds.include_point(x, y);
        let n = lines.len() as f32;
        let mut tspans = String::new();
        for (i, line) in lines.iter().enumerate() {
            let dy = if i == 0 { -(n - 1.0) * line_h / 2.0 } else { line_h };
            let text = escape_text(line);
            if max_width > 1.0 && crate::tools::viz::text::measure_px(line, font_size) > max_width {
                tspans.push_str(&format!(
                    r#"<tspan x="{x:.1}" dy="{dy:.1}" textLength="{max_width:.1}" lengthAdjust="spacingAndGlyphs">{text}</tspan>"#
                ));
            } else {
                tspans.push_str(&format!(r#"<tspan x="{x:.1}" dy="{dy:.1}">{text}</tspan>"#));
            }
        }
        self.body.push_str(&format!(r#"<text x="{x:.1}" y="{y:.1}" class="{class}">{tspans}</text>"#));
        self.body.push('\n');
    }

    pub fn line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, class: &str) {
        self.bounds.include_point(x1.max(x2), y1.max(y2));
        self.body
            .push_str(&format!(r#"<line x1="{x1:.1}" y1="{y1:.1}" x2="{x2:.1}" y2="{y2:.1}" class="{class}"/>"#));
        self.body.push('\n');
    }

    pub fn polygon(&mut self, points: &[(f32, f32)], class: &str) {
        for &(x, y) in points {
            self.bounds.include_point(x, y);
        }
        let pts: String = points.iter().map(|(x, y)| format!("{x:.1},{y:.1}")).collect::<Vec<_>>().join(" ");
        self.body.push_str(&format!(r#"<polygon points="{pts}" class="{class}"/>"#));
        self.body.push('\n');
    }

    /// Raw path data. `d` is built by the caller from already-computed
    /// coordinates (never user text), so it needs no escaping; `bbox` is the
    /// path's own extent, supplied by the caller since parsing `d` back out
    /// isn't worth the complexity for the handful of curves this crate draws.
    pub fn path(&mut self, d: &str, class: &str, bbox: (f32, f32, f32, f32)) {
        let (x, y, w, h) = bbox;
        self.bounds.include_rect(x, y, w, h);
        self.body.push_str(&format!(r#"<path d="{d}" class="{class}"/>"#));
        self.body.push('\n');
    }

    pub fn circle(&mut self, cx: f32, cy: f32, r: f32, class: &str) {
        self.bounds.include_rect(cx - r, cy - r, 2.0 * r, 2.0 * r);
        self.body.push_str(&format!(r#"<circle cx="{cx:.1}" cy="{cy:.1}" r="{r:.1}" class="{class}"/>"#));
        self.body.push('\n');
    }

    /// Appends a pre-built fragment verbatim without touching bounds or
    /// escaping. Only ever used for the shared, static, trusted `<defs>`
    /// fragment provided by `assets/defs.svg` — never for anything derived
    /// from a tool caller's input.
    pub fn raw_trusted_defs(&mut self, s: &str) {
        self.body.push_str(s);
        self.body.push('\n');
    }

    pub fn into_parts(self) -> (String, Bounds) {
        (self.body, self.bounds)
    }
}

/// Wraps a laid-out body fragment into a complete, standalone `<svg>`
/// document: the shared `<defs>`, a full-canvas background rect, the title
/// bar, and `<title>`/`<desc>` for screen readers. `width`/`height` should
/// already include `CANVAS_MARGIN`.
pub fn document(defs: &str, title: &str, description: &str, width: f32, height: f32, body: &str) -> String {
    let title_attr = escape_attr(title);
    let title_text = escape_text(title);
    let desc_text = escape_text(description);
    let cx = width / 2.0;
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width:.0} {height:.0}" width="100%" height="auto" role="img" aria-label="{title_attr}">
    <title>{title_text}</title>
    <desc>{desc_text}</desc>
    {defs}

    <rect width="100%" height="100%" class="canvas-bg"/>
    <text x="{cx:.1}" y="35" class="title-text">{title_text}</text>
    {body}
</svg>"#
    )
    .trim()
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_updates_bounds_to_its_far_corner() {
        let mut c = SvgCanvas::new();
        c.rect(10.0, 20.0, 100.0, 50.0, "node-box");
        assert_eq!(c.bounds().width(), 110.0);
        assert_eq!(c.bounds().height(), 70.0);
    }

    #[test]
    fn text_line_escapes_hostile_content() {
        let mut c = SvgCanvas::new();
        c.text_line(0.0, 0.0, "node-text", "<script>alert(1)</script>");
        let (body, _) = c.into_parts();
        assert!(!body.contains("<script>"));
        assert!(body.contains("&lt;script&gt;"));
    }

    #[test]
    fn text_line_fit_squeezes_a_too_wide_line_but_leaves_a_fit_one_alone() {
        let mut wide = SvgCanvas::new();
        wide.text_line_fit(0.0, 0.0, "node-text", "A label that is far wider than the box", 12.5, 40.0);
        let (body, _) = wide.into_parts();
        assert!(body.contains(r#"textLength="40.0""#), "too-wide line must get the textLength backstop: {body}");
        assert!(body.contains("lengthAdjust=\"spacingAndGlyphs\""));

        let mut fits = SvgCanvas::new();
        fits.text_line_fit(0.0, 0.0, "node-text", "Ok", 12.5, 40.0);
        let (body, _) = fits.into_parts();
        assert!(!body.contains("textLength"), "a fitting line must render undistorted: {body}");
    }

    #[test]
    fn text_lines_fit_applies_the_backstop_per_line() {
        let mut c = SvgCanvas::new();
        c.text_lines_fit(
            0.0,
            0.0,
            "node-text",
            &["short".to_string(), "this second line is far too wide for the given budget".to_string()],
            15.0,
            12.5,
            40.0,
        );
        let (body, _) = c.into_parts();
        assert_eq!(body.matches("textLength").count(), 1, "only the overflowing line gets clamped: {body}");
    }

    #[test]
    fn document_escapes_title_in_both_title_tag_and_aria_label() {
        let doc = document("", "A \"quoted\" & <title>", "desc", 100.0, 100.0, "");
        assert!(doc.contains("&lt;title&gt;"));
        assert!(doc.contains("&quot;quoted&quot;"));
        // Exactly one literal `<title>` opening tag -- the wrapper's own.
        // A second one would mean the user's embedded "<title>" text leaked
        // through unescaped and opened a spurious nested element.
        assert_eq!(doc.matches("<title>").count(), 1);
    }

    #[test]
    fn document_round_trips_through_quick_xml() {
        use quick_xml::events::Event;
        use quick_xml::reader::Reader;

        let doc = document("<defs></defs>", "T", "D", 200.0, 100.0, r#"<rect x="0" y="0" width="10" height="10" class="node-box"/>"#);
        let mut reader = Reader::from_str(&doc);
        loop {
            match reader.read_event() {
                Ok(Event::Eof) => break,
                Ok(_) => {}
                Err(e) => panic!("document is not well-formed XML: {e}"),
            }
        }
    }
}
