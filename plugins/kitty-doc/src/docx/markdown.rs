//! Word (`.docx`) → markdown that keeps the document's structure: headings
//! (by style, outline level, or a large all-bold line), bulleted and numbered
//! lists with their nesting (from `numbering.xml`), tables as GFM pipe tables,
//! quotes and code by style, bold/italic/strike/code runs, hyperlinks (both
//! `w:hyperlink` and `HYPERLINK` fields), footnotes and endnotes as `[^n]`,
//! text boxes after the paragraph that anchors them, and a placeholder for
//! each picture (with its description when it has one).
//!
//! Additive and separate from `read::extract_paragraphs`, which the MCP
//! tools use and whose output stays as it was. Tracked deletions, hidden
//! text, field codes, ruby glosses and `mc:Fallback` copies are left out;
//! tables of contents are dropped (their headings are already in the text).

use std::collections::HashMap;
use std::path::Path;

use crate::markdown::xml::{self, Element};
use crate::markdown::{
    escape_line_start, image_placeholder, join_cell, render_blocks, render_footnotes,
    render_inline, Block, Format, Inline,
};

use super::{read_zip_entry, DocxError, MAX_DOCX_ENTRIES};

/// A converted document.
#[derive(Debug, Clone, PartialEq)]
pub struct DocxMarkdown {
    pub markdown: String,
    /// `dc:title` from the document properties, when set.
    pub title: Option<String>,
    /// Pictures in the body (each shown as a placeholder).
    pub images: usize,
}

const MONO_FONTS: &[&str] = &[
    "courier",
    "courier new",
    "consolas",
    "menlo",
    "monaco",
    "lucida console",
    "source code pro",
    "fira code",
    "cascadia code",
    "cascadia mono",
    "dejavu sans mono",
    "liberation mono",
];

fn is_code_style(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("code")
        || n.contains("preformatted")
        || n.contains("verbatim")
        || n == "plain text"
        || n == "macro text"
}

/// A toggle property: present and not switched off.
fn on(el: Option<&Element>) -> Option<bool> {
    el.map(|e| {
        !matches!(
            e.attr("val").map(str::to_ascii_lowercase).as_deref(),
            Some("0") | Some("false") | Some("off") | Some("none")
        )
    })
}

#[derive(Debug, Default, Clone)]
struct StyleDef {
    name: String,
    based_on: Option<String>,
    outline: Option<u8>,
    num: Option<(String, u32)>,
    bold: Option<bool>,
    italic: Option<bool>,
    mono: bool,
}

#[derive(Default)]
struct Styles(HashMap<String, StyleDef>);

impl Styles {
    fn parse(xml_bytes: &[u8]) -> Self {
        let root = xml::parse(xml_bytes);
        let mut out = HashMap::new();
        let mut styles = Vec::new();
        root.find_all("style", &mut styles);
        for s in styles {
            let Some(id) = s.attr("styleId") else {
                continue;
            };
            let ppr = s.child("pPr");
            let rpr = s.child("rPr");
            let num = ppr.and_then(|p| p.child("numPr")).and_then(|n| {
                let id = n.child("numId")?.attr("val")?.to_string();
                let lvl = n
                    .child("ilvl")
                    .and_then(|l| l.attr("val"))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                Some((id, lvl))
            });
            let mono = rpr
                .and_then(|r| r.child("rFonts"))
                .and_then(|f| f.attr("ascii").or_else(|| f.attr("hAnsi")))
                .map(|f| MONO_FONTS.contains(&f.to_ascii_lowercase().as_str()))
                .unwrap_or(false);
            out.insert(
                id.to_string(),
                StyleDef {
                    name: s
                        .child("name")
                        .and_then(|n| n.attr("val"))
                        .unwrap_or(id)
                        .to_string(),
                    based_on: s
                        .child("basedOn")
                        .and_then(|b| b.attr("val"))
                        .map(str::to_string),
                    outline: ppr
                        .and_then(|p| p.child("outlineLvl"))
                        .and_then(|o| o.attr("val"))
                        .and_then(|v| v.parse().ok()),
                    num,
                    bold: on(rpr.and_then(|r| r.child("b"))),
                    italic: on(rpr.and_then(|r| r.child("i"))),
                    mono,
                },
            );
        }
        Styles(out)
    }

    /// The style and the styles it is based on, nearest first.
    fn chain(&self, id: Option<&str>) -> Vec<&StyleDef> {
        let mut out = Vec::new();
        let mut at = id.map(str::to_string);
        while let Some(id) = at {
            if out.len() >= 12 {
                break;
            }
            match self.0.get(&id) {
                Some(def) => {
                    out.push(def);
                    at = def.based_on.clone();
                }
                None => break,
            }
        }
        out
    }

    fn name(&self, id: Option<&str>) -> String {
        match id {
            Some(id) => self
                .0
                .get(id)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| id.to_string()),
            None => String::new(),
        }
    }

    fn heading_level(&self, id: Option<&str>) -> Option<u8> {
        for def in self.chain(id) {
            let name = def.name.to_ascii_lowercase();
            // House styles name their own ("OEI Title", "OEI Heading 2").
            let kicker = ["sub", "above", "toc", "table of", "caption"]
                .iter()
                .any(|w| name.contains(w));
            if name.ends_with("title") && !kicker {
                return Some(1);
            }
            if let Some(rest) = name.find("heading").map(|i| &name[i + "heading".len()..]) {
                let digits: String = rest
                    .trim()
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                if let Ok(n) = digits.parse::<u8>() {
                    if (1..=9).contains(&n) {
                        return Some(n.min(6));
                    }
                }
            }
            if let Some(o) = def.outline {
                if o < 9 {
                    return Some((o + 1).min(6));
                }
            }
        }
        None
    }

    fn numbering(&self, id: Option<&str>) -> Option<(String, u32)> {
        self.chain(id).into_iter().find_map(|d| d.num.clone())
    }

    fn bold(&self, id: Option<&str>) -> Option<bool> {
        self.chain(id).into_iter().find_map(|d| d.bold)
    }

    fn italic(&self, id: Option<&str>) -> Option<bool> {
        self.chain(id).into_iter().find_map(|d| d.italic)
    }

    fn mono(&self, id: Option<&str>) -> bool {
        self.chain(id)
            .into_iter()
            .any(|d| d.mono || is_code_style(&d.name))
    }
}

#[derive(Default)]
struct Numbering {
    nums: HashMap<String, String>,
    /// (abstractNumId, ilvl) → (numFmt, start)
    levels: HashMap<(String, u32), (String, u32)>,
}

impl Numbering {
    fn parse(xml_bytes: &[u8]) -> Self {
        let root = xml::parse(xml_bytes);
        let mut n = Numbering::default();
        let mut abstracts = Vec::new();
        root.find_all("abstractNum", &mut abstracts);
        for a in abstracts {
            let Some(aid) = a.attr("abstractNumId") else {
                continue;
            };
            for lvl in a.elements().filter(|e| e.is("lvl")) {
                let Some(ilvl) = lvl.attr("ilvl").and_then(|v| v.parse().ok()) else {
                    continue;
                };
                let fmt = lvl
                    .child("numFmt")
                    .and_then(|f| f.attr("val"))
                    .unwrap_or("decimal")
                    .to_string();
                let start = lvl
                    .child("start")
                    .and_then(|s| s.attr("val"))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1);
                n.levels.insert((aid.to_string(), ilvl), (fmt, start));
            }
        }
        let mut nums = Vec::new();
        root.find_all("num", &mut nums);
        for num in nums {
            if let (Some(id), Some(aid)) = (
                num.attr("numId"),
                num.child("abstractNumId").and_then(|a| a.attr("val")),
            ) {
                n.nums.insert(id.to_string(), aid.to_string());
            }
        }
        n
    }

    /// `None` when the level shows no marker; else whether it is a bullet and
    /// its starting number.
    fn level(&self, num_id: &str, ilvl: u32) -> Option<(bool, u32)> {
        let fmt = self
            .nums
            .get(num_id)
            .and_then(|aid| self.levels.get(&(aid.clone(), ilvl)));
        match fmt {
            Some((f, _)) if f == "none" => None,
            Some((f, _)) if f == "bullet" => Some((true, 1)),
            Some((_, start)) => Some((false, *start)),
            // Undefined numbering: a bullet is the safe reading.
            None => Some((true, 1)),
        }
    }
}

fn parse_rels(xml_bytes: &[u8]) -> HashMap<String, String> {
    let root = xml::parse(xml_bytes);
    let mut rels = Vec::new();
    root.find_all("Relationship", &mut rels);
    rels.into_iter()
        .filter(|r| r.attr("TargetMode") == Some("External"))
        .filter_map(|r| Some((r.attr("Id")?.to_string(), r.attr("Target")?.to_string())))
        .collect()
}

/// The target of a `HYPERLINK "url"` field instruction (not `\l` bookmarks).
fn hyperlink_field(instr: &str) -> Option<String> {
    let rest = instr.trim().strip_prefix("HYPERLINK")?.trim();
    if rest.starts_with("\\l") {
        return None;
    }
    let url = if let Some(stripped) = rest.strip_prefix('"') {
        stripped.split('"').next()?
    } else {
        rest.split_whitespace().next()?
    };
    (!url.is_empty()).then(|| url.to_string())
}

#[derive(Default)]
struct FieldState {
    depth: usize,
    in_instr: bool,
    instr: String,
    link: Option<String>,
}

/// What a paragraph's runs produced.
#[derive(Default)]
struct Collected<'a> {
    pieces: Vec<Inline>,
    link: Option<String>,
    boxes: Vec<&'a Element>,
    bold_chars: usize,
    plain_chars: usize,
    mono_chars: usize,
    max_size: u32,
}

impl Collected<'_> {
    fn push_text(&mut self, text: &str, format: Format, field_link: &Option<String>) {
        if text.is_empty() {
            return;
        }
        let n = text.chars().filter(|c| !c.is_whitespace()).count();
        if format.bold {
            self.bold_chars += n;
        } else {
            self.plain_chars += n;
        }
        if format.code {
            self.mono_chars += n;
        }
        let link = self.link.clone().or_else(|| field_link.clone());
        self.pieces.push(Inline::text(text, format, link));
    }

    fn plain_text(&self) -> String {
        self.pieces
            .iter()
            .map(|p| match p {
                Inline::Text { text, .. } => text.as_str(),
                Inline::Raw(_) => "",
            })
            .collect()
    }
}

struct Converter<'a> {
    styles: &'a Styles,
    numbering: &'a Numbering,
    rels: &'a HashMap<String, String>,
    footnotes: HashMap<String, &'a Element>,
    endnotes: HashMap<String, &'a Element>,
    note_labels: HashMap<(bool, String), String>,
    note_order: Vec<(bool, String, String)>,
    counters: HashMap<String, Vec<u32>>,
    field: FieldState,
    blocks: Vec<Block>,
    images: usize,
}

impl<'a> Converter<'a> {
    fn run_format(&self, rpr: Option<&Element>) -> (Format, bool) {
        let char_style = rpr
            .and_then(|r| r.child("rStyle"))
            .and_then(|s| s.attr("val"));
        let get = |name: &str| on(rpr.and_then(|r| r.child(name)));
        let bold = get("b")
            .or_else(|| self.styles.bold(char_style))
            .unwrap_or(false);
        let italic = get("i")
            .or_else(|| self.styles.italic(char_style))
            .unwrap_or(false);
        let strike = get("strike").unwrap_or(false) || get("dstrike").unwrap_or(false);
        let font_mono = rpr
            .and_then(|r| r.child("rFonts"))
            .and_then(|f| f.attr("ascii").or_else(|| f.attr("hAnsi")))
            .map(|f| MONO_FONTS.contains(&f.to_ascii_lowercase().as_str()))
            .unwrap_or(false);
        let code = font_mono || (char_style.is_some() && self.styles.mono(char_style));
        let hidden = get("vanish").unwrap_or(false) || get("specVanish").unwrap_or(false);
        (
            Format {
                bold,
                italic,
                strike,
                code,
            },
            hidden,
        )
    }

    fn note_ref(&mut self, endnote: bool, id: &str) -> Inline {
        let key = (endnote, id.to_string());
        let label = match self.note_labels.get(&key) {
            Some(l) => l.clone(),
            None => {
                let label = (self.note_labels.len() + 1).to_string();
                self.note_labels.insert(key, label.clone());
                self.note_order
                    .push((endnote, id.to_string(), label.clone()));
                label
            }
        };
        Inline::Raw(format!("[^{label}]"))
    }

    fn drawing(&mut self, d: &'a Element, out: &mut Collected<'a>) {
        let mut boxes = Vec::new();
        d.find_all("txbxContent", &mut boxes);
        if !boxes.is_empty() {
            out.boxes.extend(boxes);
            return;
        }
        if d.find("pic").is_some() || d.find("imagedata").is_some() {
            self.images += 1;
            let doc_pr = d.find("docPr");
            let desc = doc_pr
                .and_then(|p| p.attr("descr").filter(|s| !s.trim().is_empty()))
                .or_else(|| doc_pr.and_then(|p| p.attr("title")))
                .or_else(|| d.find("imagedata").and_then(|i| i.attr("title")));
            out.pieces.push(image_placeholder(desc));
        }
    }

    fn run(&mut self, r: &'a Element, out: &mut Collected<'a>) {
        let rpr = r.child("rPr");
        let (format, hidden) = self.run_format(rpr);
        if hidden {
            return;
        }
        if let Some(sz) = rpr
            .and_then(|x| x.child("sz"))
            .and_then(|s| s.attr("val"))
            .and_then(|v| v.parse::<u32>().ok())
        {
            out.max_size = out.max_size.max(sz);
        }
        self.run_children(r, format, out);
    }

    fn run_children(&mut self, r: &'a Element, format: Format, out: &mut Collected<'a>) {
        for c in r.elements() {
            match c.local() {
                "t" if !self.field.in_instr => {
                    out.push_text(&c.text(), format, &self.field.link);
                }
                "tab" | "ptab" => out.push_text("\t", format, &self.field.link),
                "br" => {
                    if !matches!(c.attr("type"), Some("page") | Some("column")) {
                        out.push_text("\n", Format::default(), &None);
                    }
                }
                "cr" => out.push_text("\n", Format::default(), &None),
                "noBreakHyphen" => out.push_text("-", format, &self.field.link),
                "fldChar" => match c.attr("fldCharType") {
                    Some("begin") => {
                        self.field.depth += 1;
                        self.field.in_instr = true;
                        self.field.instr.clear();
                    }
                    Some("separate") => {
                        self.field.in_instr = false;
                        if let Some(url) = hyperlink_field(&self.field.instr) {
                            self.field.link = Some(url);
                        }
                    }
                    Some("end") => {
                        self.field.depth = self.field.depth.saturating_sub(1);
                        self.field.in_instr = false;
                        self.field.link = None;
                    }
                    _ => {}
                },
                "instrText" if self.field.in_instr => self.field.instr.push_str(&c.text()),
                "footnoteReference" | "endnoteReference" => {
                    if let Some(id) = c.attr("id") {
                        let piece = self.note_ref(c.is("endnoteReference"), id);
                        out.pieces.push(piece);
                    }
                }
                "drawing" | "pict" | "object" => self.drawing(c, out),
                "AlternateContent" => {
                    if let Some(choice) = c.child("Choice") {
                        self.run_children(choice, format, out);
                    }
                }
                "ruby" => {
                    if let Some(base) = c.child("rubyBase") {
                        for r in base.elements().filter(|e| e.is("r")) {
                            self.run(r, out);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn inline(&mut self, el: &'a Element, out: &mut Collected<'a>) {
        match el.local() {
            "r" => self.run(el, out),
            "hyperlink" => {
                let target = el
                    .attr_q("r:id")
                    .or_else(|| el.attr("id"))
                    .and_then(|id| self.rels.get(id))
                    .cloned();
                let next = target.or_else(|| out.link.clone());
                let saved = std::mem::replace(&mut out.link, next);
                for c in el.elements() {
                    self.inline(c, out);
                }
                out.link = saved;
            }
            "fldSimple" => {
                let target = el.attr("instr").and_then(hyperlink_field);
                let saved = out.link.clone();
                if target.is_some() {
                    out.link = target;
                }
                for c in el.elements() {
                    self.inline(c, out);
                }
                out.link = saved;
            }
            "ins" | "moveTo" | "smartTag" | "customXml" | "sdtContent" | "dir" | "bdo"
            | "oMathPara" | "oMath" | "e" | "num" | "den" | "sup" | "sub" => {
                for c in el.elements() {
                    self.inline(c, out);
                }
            }
            "sdt" => {
                if let Some(content) = el.child("sdtContent") {
                    self.inline(content, out);
                }
            }
            "AlternateContent" => {
                if let Some(choice) = el.child("Choice") {
                    for c in choice.elements() {
                        self.inline(c, out);
                    }
                }
            }
            _ => {}
        }
    }

    fn collect(&mut self, p: &'a Element) -> Collected<'a> {
        let mut out = Collected::default();
        for c in p.elements() {
            self.inline(c, &mut out);
        }
        out
    }

    fn paragraph(&mut self, p: &'a Element) {
        let ppr = p.child("pPr");
        let style_id = ppr
            .and_then(|x| x.child("pStyle"))
            .and_then(|s| s.attr("val"));
        let style_name = self.styles.name(style_id).to_ascii_lowercase();
        let collected = self.collect(p);
        let boxes = collected.boxes.clone();

        // A table of contents repeats the headings with page numbers.
        let is_toc = style_name.starts_with("toc") || style_name == "table of figures";
        if !is_toc {
            self.paragraph_block(ppr, style_id, &style_name, collected);
        }
        for b in boxes {
            for c in b.elements() {
                self.block(c);
            }
        }
    }

    fn list_number(&mut self, num_id: &str, ilvl: u32, start: u32) -> u32 {
        let levels = self.counters.entry(num_id.to_string()).or_default();
        let at = ilvl as usize;
        if levels.len() <= at {
            levels.resize(at + 1, 0);
        }
        levels.truncate(at + 1);
        levels[at] = if levels[at] == 0 {
            start
        } else {
            levels[at] + 1
        };
        levels[at]
    }

    fn paragraph_block(
        &mut self,
        ppr: Option<&Element>,
        style_id: Option<&str>,
        style_name: &str,
        collected: Collected<'a>,
    ) {
        let plain = collected.plain_text();
        if plain.trim().is_empty() && !collected.pieces.iter().any(|p| matches!(p, Inline::Raw(_)))
        {
            return;
        }

        // Code: by style, or every character in a monospace font.
        if is_code_style(style_name)
            || (collected.mono_chars > 0
                && collected.mono_chars == collected.bold_chars + collected.plain_chars)
        {
            self.blocks.push(Block::Code(
                plain.replace('\u{2003}', "    ").trim_end().to_string(),
            ));
            return;
        }

        let own_outline = ppr
            .and_then(|x| x.child("outlineLvl"))
            .and_then(|o| o.attr("val"))
            .and_then(|v| v.parse::<u8>().ok())
            .filter(|o| *o < 9)
            .map(|o| (o + 1).min(6));
        let all_bold = collected.bold_chars > 0 && collected.plain_chars == 0;
        let short = plain.trim().chars().count() <= 120 && !plain.trim_end().ends_with('.');
        let size_level = if all_bold && short && !plain.contains('\n') {
            match collected.max_size {
                s if s >= 32 => Some(1),
                s if s >= 28 => Some(2),
                _ => None,
            }
        } else {
            None
        };
        let heading = self
            .styles
            .heading_level(style_id)
            .or(own_outline)
            .or(size_level);

        let mut pieces = collected.pieces;
        if heading.is_some() {
            // A heading is bold by its style; markers inside it are noise.
            for p in &mut pieces {
                if let Inline::Text { format, .. } = p {
                    format.bold = false;
                }
            }
        }
        // Tabs: dropped at the start of a line, an em space elsewhere.
        for p in &mut pieces {
            if let Inline::Text { text, .. } = p {
                if text.contains('\t') {
                    *text = text.replace('\t', "\u{2003}");
                }
            }
        }
        let rendered = render_inline(&pieces);
        let lines: Vec<String> = rendered
            .split('\n')
            .map(|l| {
                l.trim_start_matches([' ', '\u{2003}'])
                    .trim_end()
                    .to_string()
            })
            .collect();
        let text = lines
            .iter()
            .map(|l| escape_line_start(l))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();
        if text.is_empty() {
            return;
        }

        if let Some(level) = heading {
            self.blocks.push(Block::Heading {
                level,
                text: lines.join(" ").trim().to_string(),
            });
            return;
        }

        let num = ppr
            .and_then(|x| x.child("numPr"))
            .and_then(|n| {
                let id = n.child("numId")?.attr("val")?.to_string();
                let lvl = n
                    .child("ilvl")
                    .and_then(|l| l.attr("val"))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                Some((id, lvl))
            })
            .or_else(|| self.styles.numbering(style_id));
        if let Some((num_id, ilvl)) = num.filter(|(id, _)| id != "0") {
            if let Some((bullet, start)) = self.numbering.level(&num_id, ilvl) {
                let number = if bullet {
                    None
                } else {
                    Some(self.list_number(&num_id, ilvl, start))
                };
                self.blocks.push(Block::ListItem {
                    level: ilvl as usize,
                    number,
                    text,
                });
                return;
            }
        }

        if style_name.contains("quote") || style_name == "block text" {
            self.blocks.push(Block::Quote(text));
            return;
        }

        // A bullet typed by hand ("• item") reads as a list item.
        if let Some(rest) = text
            .strip_prefix(['•', '◦', '▪', '·', '‣', '○', '■', '●'])
            .map(|r| r.trim_start_matches([' ', '\u{2003}']))
        {
            if !rest.is_empty() {
                self.blocks.push(Block::ListItem {
                    level: 0,
                    number: None,
                    text: escape_line_start(rest),
                });
                return;
            }
        }

        self.blocks.push(Block::Paragraph(text));
    }

    /// A cell's blocks, converted on their own.
    fn cell_blocks(&mut self, tc: &'a Element) -> Vec<Block> {
        let saved = std::mem::take(&mut self.blocks);
        for c in tc.elements() {
            self.block(c);
        }
        std::mem::replace(&mut self.blocks, saved)
    }

    fn table(&mut self, tbl: &'a Element) {
        let mut rows: Vec<Vec<Vec<Block>>> = Vec::new();
        let mut row_els = Vec::new();
        collect_children(tbl, "tr", &mut row_els);
        for tr in row_els {
            let mut cells = Vec::new();
            let mut cell_els = Vec::new();
            collect_children(tr, "tc", &mut cell_els);
            for tc in cell_els {
                let tcpr = tc.child("tcPr");
                let span = tcpr
                    .and_then(|p| p.child("gridSpan"))
                    .and_then(|g| g.attr("val"))
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(1)
                    .clamp(1, 64);
                let merged_down = tcpr
                    .and_then(|p| p.child("vMerge"))
                    .map(|v| v.attr("val") != Some("restart"))
                    .unwrap_or(false);
                let blocks = if merged_down {
                    Vec::new()
                } else {
                    self.cell_blocks(tc)
                };
                cells.push(blocks);
                for _ in 1..span {
                    cells.push(Vec::new());
                }
            }
            if !cells.is_empty() {
                rows.push(cells);
            }
        }
        if rows.is_empty() {
            return;
        }
        let width = rows.iter().map(Vec::len).max().unwrap_or(0);
        let structured = rows.iter().flatten().any(|cell| {
            cell.len() > 3
                || cell.iter().any(|b| {
                    matches!(
                        b,
                        Block::Heading { .. } | Block::Table(_) | Block::Code(_) | Block::Quote(_)
                    )
                })
        });
        // A one-column table, or one holding headings and long passages, is a
        // layout box: its content reads as ordinary blocks.
        if width <= 1 || structured {
            for row in rows {
                for cell in row {
                    self.blocks.extend(cell);
                }
            }
            return;
        }
        let rendered: Vec<Vec<String>> = rows
            .into_iter()
            .map(|row| row.into_iter().map(|cell| cell_text(&cell)).collect())
            .collect();
        if rendered.iter().flatten().all(|c| c.trim().is_empty()) {
            return;
        }
        self.blocks.push(Block::Table(rendered));
    }

    fn block(&mut self, el: &'a Element) {
        match el.local() {
            "p" => self.paragraph(el),
            "tbl" => self.table(el),
            "sdt" => {
                if let Some(content) = el.child("sdtContent") {
                    for c in content.elements() {
                        self.block(c);
                    }
                }
            }
            "customXml" | "ins" | "moveTo" | "txbxContent" => {
                for c in el.elements() {
                    self.block(c);
                }
            }
            "AlternateContent" => {
                if let Some(choice) = el.child("Choice") {
                    for c in choice.elements() {
                        self.block(c);
                    }
                }
            }
            _ => {}
        }
    }

    fn note_text(&mut self, el: &'a Element) -> String {
        let mut parts = Vec::new();
        for p in el.elements().filter(|e| e.is("p")) {
            let collected = self.collect(p);
            let rendered = render_inline(&collected.pieces);
            let t = rendered.replace('\t', " ");
            let t = t.trim();
            if !t.is_empty() {
                parts.push(t.to_string());
            }
        }
        parts.join(" ")
    }
}

/// A short paragraph set entirely in bold, standing above ordinary text, is a
/// heading the writer made by hand ("**May 26**"). It goes one level below
/// the document's real headings. Placeholders (`<FIGURE 2.1>`), sentences
/// and runs of bold lines in a row are left as they are.
fn promote_bold_lines(blocks: &mut [Block]) {
    let deepest = blocks
        .iter()
        .filter_map(|b| match b {
            Block::Heading { level, .. } => Some(*level),
            _ => None,
        })
        .max();
    let level = deepest.map_or(2, |d| (d + 1).min(3));
    let bold_line = |b: &Block| -> Option<String> {
        let Block::Paragraph(text) = b else {
            return None;
        };
        let inner = text.strip_prefix("**")?.strip_suffix("**")?;
        let inner_t = inner.trim();
        let ok = !inner.contains("**")
            && !inner.contains('\n')
            && inner_t.chars().count() <= 80
            && inner_t.split_whitespace().count() <= 12
            && inner_t
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || "\"“'‘".contains(c))
            && !inner_t.ends_with(['.', ',', ';']);
        ok.then(|| inner_t.to_string())
    };
    let flags: Vec<Option<String>> = blocks.iter().map(bold_line).collect();
    for i in 0..blocks.len() {
        let Some(text) = flags[i].clone() else {
            continue;
        };
        let next_plain = matches!(
            blocks.get(i + 1),
            Some(Block::Paragraph(_))
                | Some(Block::ListItem { .. })
                | Some(Block::Table(_))
                | Some(Block::Quote(_))
        ) && flags.get(i + 1).is_some_and(Option::is_none);
        let prev_bold = i > 0 && flags[i - 1].is_some();
        if next_plain && !prev_bold {
            blocks[i] = Block::Heading { level, text };
        }
    }
}

/// Direct children with this name, looking through content controls.
fn collect_children<'a>(el: &'a Element, name: &str, out: &mut Vec<&'a Element>) {
    for c in el.elements() {
        if c.is(name) {
            out.push(c);
        } else if c.is("sdt") || c.is("sdtContent") || c.is("customXml") {
            collect_children(c, name, out);
        }
    }
}

/// A table cell's blocks as one line of inline markdown.
fn cell_text(blocks: &[Block]) -> String {
    let parts = blocks
        .iter()
        .map(|b| match b {
            Block::Heading { text, .. } | Block::Paragraph(text) | Block::Quote(text) => {
                text.clone()
            }
            Block::ListItem { text, .. } => format!("• {text}"),
            Block::Code(t) => format!("`{}`", t.replace('`', "'")),
            Block::Table(_) | Block::Raw(_) => String::new(),
        })
        .collect::<Vec<_>>();
    join_cell(parts)
}

fn notes_map(root: &Element, name: &str) -> HashMap<String, Element> {
    let mut found = Vec::new();
    root.find_all(name, &mut found);
    found
        .into_iter()
        .filter(|n| {
            !matches!(
                n.attr("type"),
                Some("separator") | Some("continuationSeparator") | Some("continuationNotice")
            )
        })
        .filter_map(|n| Some((n.attr("id")?.to_string(), n.clone())))
        .collect()
}

fn core_title(xml_bytes: &[u8]) -> Option<String> {
    let root = xml::parse(xml_bytes);
    let t = root.find("title")?.text();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    (!t.is_empty()).then_some(t)
}

/// Convert a `.docx` to markdown.
pub fn docx_to_markdown(path: &Path) -> Result<DocxMarkdown, DocxError> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(DocxError::NotFound),
        Err(e) => return Err(DocxError::Corrupt(e.to_string())),
    };
    let mut archive = zip::ZipArchive::new(file).map_err(|e| DocxError::Corrupt(e.to_string()))?;
    if archive.len() > MAX_DOCX_ENTRIES {
        return Err(DocxError::Corrupt(format!(
            "archive has too many entries ({})",
            archive.len()
        )));
    }
    let document = read_zip_entry(&mut archive, "word/document.xml")
        .ok_or_else(|| DocxError::Corrupt("missing word/document.xml".to_string()))?;
    let styles = read_zip_entry(&mut archive, "word/styles.xml")
        .map(|x| Styles::parse(&x))
        .unwrap_or_default();
    let numbering = read_zip_entry(&mut archive, "word/numbering.xml")
        .map(|x| Numbering::parse(&x))
        .unwrap_or_default();
    let rels = read_zip_entry(&mut archive, "word/_rels/document.xml.rels")
        .map(|x| parse_rels(&x))
        .unwrap_or_default();
    let footnotes_root = read_zip_entry(&mut archive, "word/footnotes.xml")
        .map(|x| xml::parse(&x))
        .unwrap_or_default();
    let endnotes_root = read_zip_entry(&mut archive, "word/endnotes.xml")
        .map(|x| xml::parse(&x))
        .unwrap_or_default();
    let title = read_zip_entry(&mut archive, "docProps/core.xml").and_then(|x| core_title(&x));
    Ok(convert(
        &document,
        &styles,
        &numbering,
        &rels,
        &footnotes_root,
        &endnotes_root,
        title,
    ))
}

fn convert(
    document: &[u8],
    styles: &Styles,
    numbering: &Numbering,
    rels: &HashMap<String, String>,
    footnotes_root: &Element,
    endnotes_root: &Element,
    title: Option<String>,
) -> DocxMarkdown {
    let root = xml::parse(document);
    let footnotes = notes_map(footnotes_root, "footnote");
    let endnotes = notes_map(endnotes_root, "endnote");
    let mut conv = Converter {
        styles,
        numbering,
        rels,
        footnotes: footnotes.iter().map(|(k, v)| (k.clone(), v)).collect(),
        endnotes: endnotes.iter().map(|(k, v)| (k.clone(), v)).collect(),
        note_labels: HashMap::new(),
        note_order: Vec::new(),
        counters: HashMap::new(),
        field: FieldState::default(),
        blocks: Vec::new(),
        images: 0,
    };
    if let Some(body) = root.find("body") {
        for c in body.elements() {
            conv.block(c);
        }
    }
    promote_bold_lines(&mut conv.blocks);
    let mut markdown = render_blocks(&conv.blocks);

    // Footnote text may itself reference notes; walk until none are new.
    let mut defs: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < conv.note_order.len() {
        let (endnote, id, label) = conv.note_order[i].clone();
        let el = if endnote {
            conv.endnotes.get(&id).copied()
        } else {
            conv.footnotes.get(&id).copied()
        };
        let text = el.map(|e| conv.note_text(e)).unwrap_or_default();
        defs.push((label, text));
        i += 1;
    }
    if !defs.is_empty() {
        markdown.push_str("\n\n");
        markdown.push_str(&render_footnotes(&defs));
    }
    DocxMarkdown {
        markdown,
        title,
        images: conv.images,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

    fn md(body: &str) -> String {
        md_with(body, "", "", "")
    }

    fn md_with(body: &str, styles: &str, numbering: &str, footnotes: &str) -> String {
        let doc = format!(r#"<w:document {NS}><w:body>{body}</w:body></w:document>"#);
        let styles = Styles::parse(format!(r#"<w:styles {NS}>{styles}</w:styles>"#).as_bytes());
        let numbering =
            Numbering::parse(format!(r#"<w:numbering {NS}>{numbering}</w:numbering>"#).as_bytes());
        let mut rels = HashMap::new();
        rels.insert("rId9".to_string(), "https://example.org/x".to_string());
        let foot = xml::parse(format!(r#"<w:footnotes {NS}>{footnotes}</w:footnotes>"#).as_bytes());
        convert(
            doc.as_bytes(),
            &styles,
            &numbering,
            &rels,
            &foot,
            &Element::default(),
            None,
        )
        .markdown
    }

    fn p(runs: &str) -> String {
        format!("<w:p>{runs}</w:p>")
    }

    #[test]
    fn one_bold_word_does_not_bold_the_paragraph() {
        let out = md(&p(
            r#"<w:r><w:t xml:space="preserve">A </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>bold</w:t></w:r><w:r><w:t xml:space="preserve"> word &amp; more.</w:t></w:r>"#,
        ));
        assert_eq!(out, "A **bold** word & more.");
    }

    #[test]
    fn bold_switched_off_is_not_bold() {
        let out = md(&p(
            r#"<w:r><w:rPr><w:b w:val="0"/><w:i/></w:rPr><w:t>x</w:t></w:r>"#,
        ));
        assert_eq!(out, "_x_");
    }

    #[test]
    fn heading_styles_become_headings() {
        let styles = r#"<w:style w:type="paragraph" w:styleId="Heading2"><w:name w:val="heading 2"/></w:style>"#;
        let body = r#"<w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:rPr><w:b/></w:rPr><w:t>Methods</w:t></w:r></w:p>"#;
        assert_eq!(md_with(body, styles, "", ""), "## Methods");
    }

    #[test]
    fn numbered_and_bulleted_lists_nest() {
        let numbering = r#"<w:abstractNum w:abstractNumId="0">
              <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/></w:lvl>
              <w:lvl w:ilvl="1"><w:numFmt w:val="bullet"/></w:lvl></w:abstractNum>
            <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#;
        let item = |lvl: u32, t: &str| {
            format!(
                r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="{lvl}"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>{t}</w:t></w:r></w:p>"#
            )
        };
        let body = [
            item(0, "One"),
            item(1, "Sub"),
            item(0, "Two"),
            p("<w:r><w:t>After</w:t></w:r>"),
        ]
        .concat();
        assert_eq!(
            md_with(&body, "", numbering, ""),
            "1. One\n   - Sub\n2. Two\n\nAfter"
        );
    }

    #[test]
    fn tables_become_pipe_tables() {
        let cell = |t: &str| format!("<w:tc><w:p><w:r><w:t>{t}</w:t></w:r></w:p></w:tc>");
        let body = format!(
            "<w:tbl><w:tr>{}{}</w:tr><w:tr>{}{}</w:tr></w:tbl>",
            cell("Name"),
            cell("Year"),
            cell("Propp"),
            cell("1928")
        );
        assert_eq!(
            md(&body),
            "| Name | Year |\n| --- | --- |\n| Propp | 1928 |"
        );
    }

    #[test]
    fn hyperlinks_and_footnotes_survive() {
        let body = p(
            r#"<w:hyperlink r:id="rId9"><w:r><w:t>site</w:t></w:r></w:hyperlink><w:r><w:footnoteReference w:id="2"/></w:r>"#,
        );
        let foot = r#"<w:footnote w:type="separator" w:id="0"><w:p><w:r><w:t>---</w:t></w:r></w:p></w:footnote>
            <w:footnote w:id="2"><w:p><w:r><w:footnoteRef/></w:r><w:r><w:t xml:space="preserve"> A note.</w:t></w:r></w:p></w:footnote>"#;
        assert_eq!(
            md_with(&body, "", "", foot),
            "[site](https://example.org/x)[^1]\n\n[^1]: A note."
        );
    }

    #[test]
    fn hyperlink_fields_link_their_result() {
        let body = p(
            r#"<w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText> HYPERLINK "https://a.org" </w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>A</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r>"#,
        );
        assert_eq!(md(&body), "[A](https://a.org)");
    }

    #[test]
    fn literal_markup_is_escaped_and_deletions_dropped() {
        let body = p(
            r#"<w:r><w:t>1. Not a list *really*</w:t></w:r><w:del><w:r><w:delText>gone</w:delText></w:r></w:del>"#,
        );
        assert_eq!(md(&body), "1\\. Not a list \\*really\\*");
    }

    #[test]
    fn house_heading_and_title_styles_count() {
        let styles = r#"<w:style w:type="paragraph" w:styleId="OEITitle"><w:name w:val="OEI Title"/></w:style>
            <w:style w:type="paragraph" w:styleId="OEIHeading2"><w:name w:val="OEI Heading 2"/></w:style>
            <w:style w:type="paragraph" w:styleId="Above"><w:name w:val="OEI Above Title"/></w:style>"#;
        let para = |style: &str, t: &str| {
            format!(
                r#"<w:p><w:pPr><w:pStyle w:val="{style}"/></w:pPr><w:r><w:t>{t}</w:t></w:r></w:p>"#
            )
        };
        let body = [
            para("Above", "Week 3"),
            para("OEITitle", "Memes"),
            para("OEIHeading2", "Review"),
        ]
        .concat();
        assert_eq!(
            md_with(&body, styles, "", ""),
            "Week 3\n\n# Memes\n\n## Review"
        );
    }

    #[test]
    fn a_short_bold_line_over_text_is_a_heading() {
        let bold = |t: &str| {
            p(&format!(
                r#"<w:r><w:rPr><w:b/></w:rPr><w:t>{t}</w:t></w:r>"#
            ))
        };
        let plain = |t: &str| p(&format!("<w:r><w:t>{t}</w:t></w:r>"));
        let body = [
            bold("May 26"),
            plain("Welcome to the course."),
            bold("A whole bold sentence ends here."),
            plain("More."),
            bold("Name"),
            bold("Address"),
            plain("Text."),
        ]
        .concat();
        assert_eq!(
            md(&body),
            "## May 26\n\nWelcome to the course.\n\n**A whole bold sentence ends here.**\n\nMore.\n\n**Name**\n\n**Address**\n\nText."
        );
    }

    #[test]
    fn pictures_leave_a_placeholder() {
        let body = p(
            r#"<w:r><w:drawing><wp:inline xmlns:wp="x"><wp:docPr id="1" name="Picture 1" descr="A map of Ireland"/><a:graphic xmlns:a="y"><a:graphicData><pic:pic xmlns:pic="z"/></a:graphicData></a:graphic></wp:inline></w:drawing></w:r>"#,
        );
        assert_eq!(md(&body), "_\\[Image: A map of Ireland\\]_");
    }
}
