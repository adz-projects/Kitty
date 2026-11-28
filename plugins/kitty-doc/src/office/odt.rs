//! OpenDocument text (`.odt`) → markdown: headings by outline level, lists
//! (bulleted or numbered per their list style) with nesting, tables, quotes
//! and preformatted text by style, bold/italic/strike/monospace spans, links,
//! footnotes and endnotes, text frames after their paragraph, and a
//! placeholder for each picture. Tracked deletions, annotations and generated
//! indexes (tables of contents) are left out.

use std::collections::HashMap;
use std::path::Path;

use crate::markdown::xml::{self, Element, Node};
use crate::markdown::{
    escape_line_start, image_placeholder, join_cell, render_blocks, render_footnotes,
    render_inline, Block, Format, Inline,
};

use super::{entry, meta_title, open};

#[derive(Debug, Clone, PartialEq)]
pub struct OdtMarkdown {
    pub markdown: String,
    pub title: Option<String>,
    pub images: usize,
}

const MONO: &[&str] = &[
    "courier",
    "courier new",
    "consolas",
    "liberation mono",
    "dejavu sans mono",
    "menlo",
    "monaco",
    "source code pro",
];

#[derive(Default, Clone)]
struct StyleDef {
    display: String,
    parent: Option<String>,
    bold: Option<bool>,
    italic: Option<bool>,
    strike: Option<bool>,
    mono: Option<bool>,
}

#[derive(Default)]
struct Styles {
    styles: HashMap<String, StyleDef>,
    /// List style name → per level (1-based): numbered?
    lists: HashMap<String, HashMap<u32, bool>>,
    /// Font face name → family, for `style:font-name` references.
    fonts: HashMap<String, String>,
}

impl Styles {
    fn absorb(&mut self, root: &Element) {
        let mut faces = Vec::new();
        root.find_all("font-face", &mut faces);
        for f in faces {
            if let Some(name) = f.attr("name") {
                let family = f.attr("font-family").unwrap_or(name);
                self.fonts.insert(
                    name.to_string(),
                    family.trim_matches(['\'', '"']).to_ascii_lowercase(),
                );
            }
        }
        let mut styles = Vec::new();
        root.find_all("style", &mut styles);
        for s in styles {
            let Some(name) = s.attr_q("style:name") else {
                continue;
            };
            let tp = s.child("text-properties");
            let prop = |k: &str| tp.and_then(|t| t.attr_q(k));
            let font = prop("style:font-name").and_then(|f| self.fonts.get(f).cloned());
            self.styles.insert(
                name.to_string(),
                StyleDef {
                    display: s
                        .attr_q("style:display-name")
                        .unwrap_or(name)
                        .replace("_20_", " ")
                        .to_ascii_lowercase(),
                    parent: s.attr_q("style:parent-style-name").map(str::to_string),
                    bold: prop("fo:font-weight")
                        .map(|w| w == "bold" || w.parse::<u32>().is_ok_and(|n| n >= 600)),
                    italic: prop("fo:font-style").map(|v| v == "italic" || v == "oblique"),
                    strike: prop("style:text-line-through-style").map(|v| v != "none"),
                    mono: font.map(|f| MONO.contains(&f.as_str())),
                },
            );
        }
        let mut lists = Vec::new();
        root.find_all("list-style", &mut lists);
        for l in lists {
            let Some(name) = l.attr_q("style:name") else {
                continue;
            };
            let mut levels = HashMap::new();
            for lvl in l.elements() {
                let Some(level) = lvl.attr("level").and_then(|v| v.parse().ok()) else {
                    continue;
                };
                let numbered = lvl.is("list-level-style-number")
                    && lvl
                        .attr_q("style:num-format")
                        .is_some_and(|f| !f.is_empty());
                levels.insert(level, numbered);
            }
            self.lists.insert(name.to_string(), levels);
        }
    }

    fn chain(&self, name: Option<&str>) -> Vec<&StyleDef> {
        let mut out = Vec::new();
        let mut at = name.map(str::to_string);
        while let Some(n) = at {
            if out.len() >= 12 {
                break;
            }
            match self.styles.get(&n) {
                Some(d) => {
                    out.push(d);
                    at = d.parent.clone();
                }
                None => break,
            }
        }
        out
    }

    fn format(&self, name: Option<&str>, base: Format) -> Format {
        let chain = self.chain(name);
        let pick = |f: fn(&StyleDef) -> Option<bool>, dflt: bool| {
            chain.iter().find_map(|d| f(d)).unwrap_or(dflt)
        };
        Format {
            bold: pick(|d| d.bold, base.bold),
            italic: pick(|d| d.italic, base.italic),
            strike: pick(|d| d.strike, base.strike),
            code: pick(|d| d.mono, base.code),
        }
    }

    fn names(&self, name: Option<&str>) -> Vec<String> {
        self.chain(name).iter().map(|d| d.display.clone()).collect()
    }
}

#[derive(Default)]
struct Collected<'a> {
    pieces: Vec<Inline>,
    frames: Vec<&'a Element>,
}

struct Converter<'a> {
    styles: &'a Styles,
    blocks: Vec<Block>,
    notes: Vec<(String, String)>,
    images: usize,
    counters: Vec<u32>,
}

fn normalize_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if matches!(c, ' ' | '\t' | '\n' | '\r') {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
}

impl<'a> Converter<'a> {
    fn inline(
        &mut self,
        el: &'a Element,
        format: Format,
        link: &Option<String>,
        out: &mut Collected<'a>,
    ) {
        for child in &el.children {
            match child {
                Node::Text(t) => {
                    out.pieces
                        .push(Inline::text(normalize_ws(t), format, link.clone()));
                }
                Node::Element(c) => match c.local() {
                    "span" => {
                        let f = self.styles.format(c.attr_q("text:style-name"), format);
                        self.inline(c, f, link, out);
                    }
                    "a" => {
                        let href = c.attr_q("xlink:href").map(str::to_string);
                        let href = href.filter(|h| !h.starts_with('#'));
                        self.inline(c, format, &href.or(link.clone()), out);
                    }
                    "s" => {
                        let n = c.attr("c").and_then(|v| v.parse().ok()).unwrap_or(1usize);
                        out.pieces
                            .push(Inline::text(" ".repeat(n.min(64)), format, link.clone()));
                    }
                    "tab" => out
                        .pieces
                        .push(Inline::text("\u{2003}", format, link.clone())),
                    "line-break" => out.pieces.push(Inline::text("\n", Format::default(), None)),
                    "note" => {
                        let label = (self.notes.len() + 1).to_string();
                        let body = c
                            .child("note-body")
                            .map(|b| self.note_text(b))
                            .unwrap_or_default();
                        self.notes.push((label.clone(), body));
                        out.pieces.push(Inline::Raw(format!("[^{label}]")));
                    }
                    "frame" => {
                        if c.child("text-box").is_some() {
                            out.frames.push(c);
                        } else if c.child("image").is_some() || c.child("object").is_some() {
                            self.images += 1;
                            let desc = c
                                .child("desc")
                                .or_else(|| c.child("title"))
                                .map(|d| d.text());
                            out.pieces.push(image_placeholder(desc.as_deref()));
                        }
                    }
                    "ruby" => {
                        if let Some(base) = c.child("ruby-base") {
                            self.inline(base, format, link, out);
                        }
                    }
                    "annotation"
                    | "annotation-end"
                    | "bookmark"
                    | "bookmark-start"
                    | "bookmark-end"
                    | "reference-mark"
                    | "reference-mark-start"
                    | "reference-mark-end"
                    | "change"
                    | "change-start"
                    | "change-end"
                    | "soft-page-break"
                    | "note-citation"
                    | "toc-mark"
                    | "alphabetical-index-mark" => {}
                    _ => self.inline(c, format, link, out),
                },
            }
        }
    }

    fn collect(&mut self, p: &'a Element) -> Collected<'a> {
        let mut out = Collected::default();
        // A paragraph style's own emphasis is the paragraph's look, not marked
        // text; only spans carry emphasis into the markdown.
        self.inline(p, Format::default(), &None, &mut out);
        out
    }

    fn rendered(pieces: &[Inline]) -> String {
        render_inline(pieces)
            .split('\n')
            .map(|l| escape_line_start(l.trim_matches([' ', '\u{2003}'])))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    }

    fn plain(pieces: &[Inline]) -> String {
        pieces
            .iter()
            .map(|p| match p {
                Inline::Text { text, .. } => text.as_str(),
                Inline::Raw(_) => "",
            })
            .collect()
    }

    fn after_frames(&mut self, frames: Vec<&'a Element>) {
        for f in frames {
            if let Some(tb) = f.child("text-box") {
                for c in tb.elements() {
                    self.block(c);
                }
            }
        }
    }

    fn heading(&mut self, h: &'a Element) {
        let level = h
            .attr("outline-level")
            .and_then(|v| v.parse::<u8>().ok())
            .unwrap_or(1)
            .clamp(1, 6);
        let mut c = self.collect(h);
        for p in &mut c.pieces {
            if let Inline::Text { format, .. } = p {
                format.bold = false;
            }
        }
        let text = Self::rendered(&c.pieces).replace('\n', " ");
        if !text.is_empty() {
            self.blocks.push(Block::Heading { level, text });
        }
        self.after_frames(c.frames);
    }

    fn paragraph(&mut self, p: &'a Element) {
        let names = self.styles.names(p.attr_q("text:style-name"));
        let c = self.collect(p);
        let has_raw = c.pieces.iter().any(|x| matches!(x, Inline::Raw(_)));
        let plain = Self::plain(&c.pieces);
        if !plain.trim().is_empty() || has_raw {
            if names
                .iter()
                .any(|n| n.contains("preformatted") || n.contains("code"))
            {
                self.blocks.push(Block::Code(plain.trim_end().to_string()));
            } else {
                let text = Self::rendered(&c.pieces);
                if names.iter().any(|n| n.contains("quot")) {
                    self.blocks.push(Block::Quote(text));
                } else if let Some(level) = names.iter().find_map(|n| {
                    n.strip_prefix("heading ")
                        .and_then(|d| d.trim().parse::<u8>().ok())
                }) {
                    self.blocks.push(Block::Heading {
                        level: level.clamp(1, 6),
                        text: text.replace('\n', " "),
                    });
                } else if names.iter().any(|n| n == "title") {
                    self.blocks.push(Block::Heading {
                        level: 1,
                        text: text.replace('\n', " "),
                    });
                } else {
                    self.blocks.push(Block::Paragraph(text));
                }
            }
        }
        self.after_frames(c.frames);
    }

    fn list(&mut self, list: &'a Element, level: usize, style: Option<&'a str>) {
        let style = list.attr_q("text:style-name").or(style);
        if level == 0 && list.attr_q("text:continue-numbering") != Some("true") {
            self.counters.clear();
        }
        let numbered = style
            .and_then(|s| self.styles.lists.get(s))
            .and_then(|l| l.get(&(level as u32 + 1)).copied())
            .unwrap_or(false);
        for item in list.elements() {
            let header = item.is("list-header");
            if !item.is("list-item") && !header {
                continue;
            }
            let mut texts: Vec<String> = Vec::new();
            let mut frames = Vec::new();
            let mut nested: Vec<&'a Element> = Vec::new();
            for c in item.elements() {
                match c.local() {
                    "p" | "h" => {
                        let col = self.collect(c);
                        let t = Self::rendered(&col.pieces);
                        if !t.is_empty() {
                            texts.push(t);
                        }
                        frames.extend(col.frames);
                    }
                    "list" => nested.push(c),
                    _ => {}
                }
            }
            if !texts.is_empty() {
                let number = if numbered && !header {
                    if self.counters.len() <= level {
                        self.counters.resize(level + 1, 0);
                    }
                    self.counters.truncate(level + 1);
                    let start = item.attr_q("text:start-value").and_then(|v| v.parse().ok());
                    self.counters[level] = start.unwrap_or(self.counters[level] + 1);
                    Some(self.counters[level])
                } else {
                    None
                };
                self.blocks.push(Block::ListItem {
                    level,
                    number,
                    text: texts.join("\n"),
                });
            }
            self.after_frames(frames);
            for n in nested {
                self.list(n, level + 1, style);
            }
        }
    }

    fn table(&mut self, t: &'a Element) {
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut row_els = Vec::new();
        collect_rows(t, &mut row_els);
        for r in row_els {
            let mut cells = Vec::new();
            for c in r.elements() {
                if !(c.is("table-cell") || c.is("covered-table-cell")) {
                    continue;
                }
                let repeat = c
                    .attr("number-columns-repeated")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(1)
                    .clamp(1, 64);
                let text = if c.is("covered-table-cell") {
                    String::new()
                } else {
                    let saved = std::mem::take(&mut self.blocks);
                    for x in c.elements() {
                        self.block(x);
                    }
                    let inner = std::mem::replace(&mut self.blocks, saved);
                    let inner = inner
                        .iter()
                        .map(|b| match b {
                            Block::Heading { text, .. }
                            | Block::Paragraph(text)
                            | Block::Quote(text) => text.clone(),
                            Block::ListItem { text, .. } => format!("• {text}"),
                            Block::Code(t) => format!("`{t}`"),
                            _ => String::new(),
                        })
                        .collect::<Vec<_>>();
                    join_cell(inner)
                };
                for _ in 0..repeat {
                    cells.push(text.clone());
                }
            }
            // Trailing empty cells (spreadsheet-style repeats) carry nothing.
            while cells.last().is_some_and(|c| c.trim().is_empty()) {
                cells.pop();
            }
            rows.push(cells);
        }
        while rows.last().is_some_and(|r| r.is_empty()) {
            rows.pop();
        }
        if rows.iter().flatten().all(|c| c.trim().is_empty()) {
            return;
        }
        let width = rows.iter().map(Vec::len).max().unwrap_or(0);
        if width <= 1 {
            for r in rows {
                for c in r {
                    if !c.trim().is_empty() {
                        self.blocks.push(Block::Paragraph(c));
                    }
                }
            }
        } else {
            self.blocks.push(Block::Table(rows));
        }
    }

    fn block(&mut self, el: &'a Element) {
        match el.local() {
            "h" => self.heading(el),
            "p" => self.paragraph(el),
            "list" => self.list(el, 0, None),
            "table" => self.table(el),
            "section" | "text" | "index-body" => {
                for c in el.elements() {
                    self.block(c);
                }
            }
            _ => {}
        }
    }

    fn note_text(&mut self, body: &'a Element) -> String {
        let saved = std::mem::take(&mut self.blocks);
        for c in body.elements() {
            self.block(c);
        }
        let inner = std::mem::replace(&mut self.blocks, saved);
        inner
            .iter()
            .map(|b| match b {
                Block::Heading { text, .. }
                | Block::Paragraph(text)
                | Block::Quote(text)
                | Block::ListItem { text, .. } => text.replace('\n', " "),
                _ => String::new(),
            })
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn collect_rows<'a>(el: &'a Element, out: &mut Vec<&'a Element>) {
    for c in el.elements() {
        match c.local() {
            "table-row" => out.push(c),
            "table-header-rows" | "table-rows" | "table-row-group" => collect_rows(c, out),
            _ => {}
        }
    }
}

fn convert(content: &[u8], styles_part: Option<&[u8]>, title: Option<String>) -> OdtMarkdown {
    let root = xml::parse(content);
    let mut styles = Styles::default();
    if let Some(s) = styles_part {
        styles.absorb(&xml::parse(s));
    }
    styles.absorb(&root);
    let mut conv = Converter {
        styles: &styles,
        blocks: Vec::new(),
        notes: Vec::new(),
        images: 0,
        counters: Vec::new(),
    };
    if let Some(text) = root.find("body").and_then(|b| b.child("text")) {
        for c in text.elements() {
            conv.block(c);
        }
    }
    let mut markdown = render_blocks(&conv.blocks);
    if !conv.notes.is_empty() {
        markdown.push_str("\n\n");
        markdown.push_str(&render_footnotes(&conv.notes));
    }
    OdtMarkdown {
        markdown,
        title,
        images: conv.images,
    }
}

/// Convert an `.odt` to markdown.
pub fn odt_to_markdown(path: &Path) -> Result<OdtMarkdown, String> {
    let mut archive = open(path)?;
    let content =
        entry(&mut archive, "content.xml").ok_or_else(|| "missing content.xml".to_string())?;
    let styles = entry(&mut archive, "styles.xml");
    let title = entry(&mut archive, "meta.xml").and_then(|x| meta_title(&x));
    Ok(convert(&content, styles.as_deref(), title))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(auto_styles: &str, body: &str) -> String {
        let content = format!(
            r#"<office:document-content xmlns:office="o" xmlns:text="t" xmlns:style="s" xmlns:fo="f" xmlns:table="tb" xmlns:xlink="x"><office:automatic-styles>{auto_styles}</office:automatic-styles><office:body><office:text>{body}</office:text></office:body></office:document-content>"#
        );
        convert(content.as_bytes(), None, None).markdown
    }

    #[test]
    fn headings_spans_links_and_spaces() {
        let styles = r#"<style:style style:name="T1" style:family="text"><style:text-properties fo:font-weight="bold"/></style:style>"#;
        let body = r#"<text:h text:outline-level="2">Intro</text:h><text:p>A <text:span text:style-name="T1">bold</text:span>   word,<text:s text:c="2"/>a <text:a xlink:href="https://x.org">link</text:a> &amp; more.</text:p>"#;
        assert_eq!(
            md(styles, body),
            "## Intro\n\nA **bold** word,  a [link](https://x.org) & more."
        );
    }

    #[test]
    fn numbered_lists_nest() {
        let styles = r#"<text:list-style style:name="L1"><text:list-level-style-number text:level="1" style:num-format="1"/><text:list-level-style-bullet text:level="2"/></text:list-style>"#;
        let body = r#"<text:list text:style-name="L1"><text:list-item><text:p>One</text:p><text:list><text:list-item><text:p>Sub</text:p></text:list-item></text:list></text:list-item><text:list-item><text:p>Two</text:p></text:list-item></text:list>"#;
        assert_eq!(md(styles, body), "1. One\n   - Sub\n2. Two");
    }

    #[test]
    fn footnotes_and_tables() {
        let body = r#"<text:p>Claim<text:note text:note-class="footnote"><text:note-citation>1</text:note-citation><text:note-body><text:p>Source.</text:p></text:note-body></text:note>.</text:p><table:table><table:table-row><table:table-cell><text:p>A</text:p></table:table-cell><table:table-cell><text:p>B</text:p></table:table-cell></table:table-row></table:table>"#;
        assert_eq!(
            md("", body),
            "Claim[^1].\n\n| A | B |\n| --- | --- |\n\n[^1]: Source."
        );
    }
}
