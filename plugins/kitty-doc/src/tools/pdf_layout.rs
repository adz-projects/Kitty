//! Where each line of a PDF's text layer sits: its box and type size.
//!
//! The text tools read a page as plain text, which is all a model needs. A
//! reader that wants to rebuild the page — paragraphs from line spacing and
//! indents, headings from type size, running heads from their position, and
//! two columns read in order — needs the geometry the plain text throws away.
//! This collects it from the same extractor (`pdf-extract`, through its
//! `OutputDev` callbacks), one page at a time under `catch_unwind`, so one bad
//! page costs that page.
//!
//! Additive, for Bacon's source importer; the MCP tools do not use it.
//!
//! Coordinates are PDF points with the origin at the page's top left. A
//! "line" is a run of glyphs on one baseline with no gap wider than about a
//! character: two columns set side by side come back as two lines, which is
//! what lets a caller find the gutter.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

use pdf_extract::{MediaBox, OutputDev, OutputError, Transform};

/// One line of text and where it sits.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutLine {
    pub x0: f64,
    pub x1: f64,
    /// The baseline, from the top of the page.
    pub baseline: f64,
    /// The type size most of the line is set in, in points.
    pub size: f64,
    pub text: String,
}

impl LayoutLine {
    pub fn top(&self) -> f64 {
        self.baseline - self.size * 0.8
    }
    pub fn bottom(&self) -> f64 {
        self.baseline + self.size * 0.25
    }
}

/// A page's lines in the order the PDF draws them (which is usually, but not
/// always, reading order).
#[derive(Debug, Clone, PartialEq)]
pub struct PageLayout {
    /// 1-based.
    pub page: u32,
    pub width: f64,
    pub height: f64,
    pub lines: Vec<LayoutLine>,
}

#[derive(Debug, Clone)]
struct Glyph {
    x: f64,
    end: f64,
    baseline: f64,
    size: f64,
    text: String,
}

#[derive(Default)]
struct Collector {
    llx: f64,
    ury: f64,
    width: f64,
    height: f64,
    glyphs: Vec<Glyph>,
}

fn expand_ligature(c: &str) -> &str {
    match c {
        "\u{FB00}" => "ff",
        "\u{FB01}" => "fi",
        "\u{FB02}" => "fl",
        "\u{FB03}" => "ffi",
        "\u{FB04}" => "ffl",
        "\u{FB05}" | "\u{FB06}" => "st",
        "\u{A0}" | "\t" => " ",
        _ => c,
    }
}

impl OutputDev for Collector {
    fn begin_page(
        &mut self,
        _page_num: u32,
        media_box: &MediaBox,
        _art_box: Option<(f64, f64, f64, f64)>,
    ) -> Result<(), OutputError> {
        self.llx = media_box.llx;
        self.ury = media_box.ury;
        self.width = (media_box.urx - media_box.llx).abs();
        self.height = (media_box.ury - media_box.lly).abs();
        Ok(())
    }

    fn end_page(&mut self) -> Result<(), OutputError> {
        Ok(())
    }

    fn output_character(
        &mut self,
        trm: &Transform,
        width: f64,
        _spacing: f64,
        font_size: f64,
        char: &str,
    ) -> Result<(), OutputError> {
        // Upright text only: a rotated run (a watermark or a notice set up the
        // margin) has no lines to read across, and comes out a glyph per line.
        let upright = trm.m11 > 0.0
            && trm.m22 > 0.0
            && trm.m12.abs() <= trm.m11 * 0.2
            && trm.m21.abs() <= trm.m22 * 0.2;
        if !upright {
            return Ok(());
        }
        // The same size measure pdf-extract's own text output uses.
        let vx = font_size * (trm.m11 + trm.m21);
        let vy = font_size * (trm.m12 + trm.m22);
        let size = (vx * vy).abs().sqrt();
        if !size.is_finite() || size <= 0.0 {
            return Ok(());
        }
        let x = trm.m31 - self.llx;
        let baseline = self.ury - trm.m32;
        self.glyphs.push(Glyph {
            x,
            end: x + width * size,
            baseline,
            size,
            text: expand_ligature(char).to_string(),
        });
        Ok(())
    }

    fn begin_word(&mut self) -> Result<(), OutputError> {
        Ok(())
    }

    fn end_word(&mut self) -> Result<(), OutputError> {
        Ok(())
    }

    fn end_line(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct Word {
    x0: f64,
    end: f64,
    text: String,
    sizes: Vec<(f64, usize)>,
}

#[derive(Debug)]
struct Building {
    baseline: f64,
    words: Vec<Word>,
    /// The next glyph starts a new word (a space was drawn).
    space: bool,
}

/// The size most characters are set in (a superscript does not count).
fn main_size(sizes: &[(f64, usize)]) -> f64 {
    let mut by: Vec<(f64, usize)> = Vec::new();
    for (s, n) in sizes {
        let r = (s * 2.0).round() / 2.0;
        match by.iter_mut().find(|(k, _)| (*k - r).abs() < 0.01) {
            Some(e) => e.1 += n,
            None => by.push((r, *n)),
        }
    }
    by.into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(a.0.total_cmp(&b.0)))
        .map(|(s, _)| s)
        .unwrap_or(0.0)
}

impl Building {
    fn start(g: &Glyph) -> Self {
        Building {
            baseline: g.baseline,
            words: vec![Word {
                x0: g.x,
                end: g.end,
                text: g.text.clone(),
                sizes: vec![(g.size, g.text.chars().count())],
            }],
            space: false,
        }
    }

    fn end(&self) -> f64 {
        self.words.last().map_or(0.0, |w| w.end)
    }

    fn last_size(&self) -> f64 {
        self.words
            .last()
            .and_then(|w| w.sizes.last())
            .map_or(0.0, |s| s.0)
    }

    fn push(&mut self, g: &Glyph, gap: f64, size: f64) {
        let new_word = self.space || gap > size * 0.15;
        self.space = false;
        match self.words.last_mut() {
            Some(w) if !new_word => {
                w.text.push_str(&g.text);
                w.end = w.end.max(g.end);
                w.sizes.push((g.size, g.text.chars().count()));
            }
            _ => self.words.push(Word {
                x0: g.x,
                end: g.end,
                text: g.text.clone(),
                sizes: vec![(g.size, g.text.chars().count())],
            }),
        }
    }

    /// The finished line, split where a gap is far wider than the line's
    /// other word gaps: justified text spaces its words evenly, so a gap that
    /// stands out is a gutter between two columns, however narrow.
    fn finish(self) -> Vec<LayoutLine> {
        let words: Vec<Word> = self
            .words
            .into_iter()
            .filter(|w| !w.text.trim().is_empty())
            .collect();
        if words.is_empty() {
            return Vec::new();
        }
        let gaps: Vec<f64> = words.windows(2).map(|w| w[1].x0 - w[0].end).collect();
        let size = main_size(
            &words
                .iter()
                .flat_map(|w| w.sizes.clone())
                .collect::<Vec<_>>(),
        );
        let mut cuts: Vec<usize> = Vec::new();
        for (i, g) in gaps.iter().enumerate() {
            let mut others: Vec<f64> = gaps
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, x)| *x)
                .collect();
            if others.len() < 2 {
                continue;
            }
            others.sort_by(f64::total_cmp);
            let median = others[others.len() / 2].max(size * 0.15);
            if *g > size * 0.7 && *g > median * 2.5 {
                cuts.push(i + 1);
            }
        }
        let mut out = Vec::new();
        let mut start = 0;
        for end in cuts.into_iter().chain(std::iter::once(words.len())) {
            let part = &words[start..end];
            start = end;
            if part.is_empty() {
                continue;
            }
            let text = part
                .iter()
                .map(|w| w.text.trim())
                .collect::<Vec<_>>()
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if text.is_empty() {
                continue;
            }
            let sizes: Vec<(f64, usize)> = part.iter().flat_map(|w| w.sizes.clone()).collect();
            out.push(LayoutLine {
                x0: part[0].x0,
                x1: part.iter().map(|w| w.end).fold(f64::MIN, f64::max),
                baseline: self.baseline,
                size: main_size(&sizes),
                text,
            });
        }
        out
    }
}

/// Glyphs in drawing order → lines. A glyph continues the current line when
/// it sits on (about) the same baseline just to the right of it; a gap wider
/// than about a character, a step back left, or a new baseline starts a new
/// line, and a line is split again at a gutter (`Building::finish`).
fn build_lines(glyphs: &[Glyph]) -> Vec<LayoutLine> {
    let mut out = Vec::new();
    let mut cur: Option<Building> = None;
    for g in glyphs {
        let blank = g.text.trim().is_empty();
        if let Some(b) = cur.as_mut() {
            // Measured against the larger of the two, so a superscript joins.
            let size = g.size.max(b.last_size()).max(1.0);
            let same_base = (g.baseline - b.baseline).abs() < size * 0.45;
            let gap = g.x - b.end();
            if same_base && gap > -size * 0.6 && gap < size * 1.2 {
                if blank {
                    b.space = true;
                    continue;
                }
                b.push(g, gap, size);
                continue;
            }
            if blank {
                continue;
            }
            if let Some(done) = cur.take() {
                out.extend(done.finish());
            }
        }
        if !blank {
            cur = Some(Building::start(g));
        }
    }
    if let Some(done) = cur {
        out.extend(done.finish());
    }
    out
}

fn page_layout(doc: &pdf_extract::Document, page: u32) -> Option<PageLayout> {
    let mut c = Collector::default();
    let ok = catch_unwind(AssertUnwindSafe(|| {
        pdf_extract::output_doc_page(doc, &mut c, page).is_ok()
    }))
    .unwrap_or(false);
    if !ok && c.glyphs.is_empty() {
        return None;
    }
    Some(PageLayout {
        page,
        width: c.width,
        height: c.height,
        lines: build_lines(&c.glyphs),
    })
}

/// Every page's lines with their boxes and sizes, in page order. A page the
/// extractor cannot read comes back with no lines (the caller falls back to
/// the plain text for it).
pub fn extract_pdf_layout(path: &Path) -> Result<Vec<PageLayout>, String> {
    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > super::pdf::PDF_MAX_FILE_BYTES {
        return Err("PDF exceeds the read limit".to_string());
    }
    let doc = super::pdf::open(path).map_err(|e| format!("could not open the PDF: {e}"))?;
    if doc.trailer.get(b"Encrypt").is_ok() {
        return Err("the PDF is password protected".to_string());
    }
    let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
    Ok(pages
        .into_iter()
        .map(|p| {
            page_layout(&doc, p).unwrap_or(PageLayout {
                page: p,
                width: 0.0,
                height: 0.0,
                lines: Vec::new(),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(x: f64, baseline: f64, text: &str) -> Glyph {
        Glyph {
            x,
            end: x + 5.0,
            baseline,
            size: 10.0,
            text: text.to_string(),
        }
    }

    #[test]
    fn glyphs_group_into_lines_and_a_gutter_splits_them() {
        let glyphs = vec![
            g(10.0, 100.0, "a"),
            g(15.0, 100.0, "b"),
            g(22.0, 100.0, "c"), // a word space before it
            // a gutter
            g(300.0, 100.0, "R"),
            g(10.0, 112.0, "n"),
        ];
        let lines = build_lines(&glyphs);
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["ab c", "R", "n"]);
        assert_eq!(lines[0].size, 10.0);
        assert_eq!(lines[0].x0, 10.0);
    }

    #[test]
    fn a_narrow_gutter_still_splits_justified_columns() {
        // Words 2pt apart, then a 9pt gutter (under the 1.2em line break).
        let mut glyphs = Vec::new();
        let mut x = 10.0;
        for w in ["one", "two", "three"] {
            glyphs.push(Glyph {
                x,
                end: x + 15.0,
                baseline: 100.0,
                size: 10.0,
                text: w.into(),
            });
            x += 17.0;
        }
        x += 7.0;
        for w in ["four", "five", "six"] {
            glyphs.push(Glyph {
                x,
                end: x + 15.0,
                baseline: 100.0,
                size: 10.0,
                text: w.into(),
            });
            x += 17.0;
        }
        let lines = build_lines(&glyphs);
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["one two three", "four five six"]);
    }

    #[test]
    fn a_superscript_does_not_set_the_line_size() {
        let mut glyphs = vec![
            g(10.0, 100.0, "w"),
            g(15.0, 100.0, "o"),
            g(20.0, 100.0, "rd"),
        ];
        glyphs.push(Glyph {
            x: 25.0,
            end: 28.0,
            baseline: 97.0,
            size: 6.0,
            text: "1".to_string(),
        });
        let lines = build_lines(&glyphs);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "word1");
        assert_eq!(lines[0].size, 10.0);
    }
}
