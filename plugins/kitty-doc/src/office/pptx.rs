//! PowerPoint (`.pptx`) → markdown, one entry per slide in presentation
//! order: the slide's title as a heading, its text boxes top to bottom (body
//! placeholders as bulleted lists at their outline level, `buNone` and plain
//! text boxes as paragraphs, numbered bullets as numbered lists), tables as
//! pipe tables, a placeholder for each described picture, and the speaker
//! notes. Date, footer and slide-number placeholders are left out.

use std::collections::HashMap;
use std::path::Path;

use crate::markdown::xml::{self, Element};
use crate::markdown::{
    escape_line_start, image_placeholder, join_cell, render_blocks, render_inline, Block, Format,
    Inline,
};

use super::{entry, meta_title, open, resolve_target, Archive};

/// A converted presentation: `(slide number, markdown)` per slide.
#[derive(Debug, Clone, PartialEq)]
pub struct PptxMarkdown {
    pub slides: Vec<(u32, String)>,
    pub title: Option<String>,
}

struct Rels {
    /// Id → resolved part path (internal) or URL (external).
    targets: HashMap<String, String>,
    /// Type suffix (`notesSlide`) → resolved part path.
    by_type: HashMap<String, String>,
}

fn rels(archive: &mut Archive, part: &str) -> Rels {
    let (dir, file) = part.rsplit_once('/').unwrap_or(("", part));
    let rels_path = format!("{dir}/_rels/{file}.rels");
    let mut out = Rels {
        targets: HashMap::new(),
        by_type: HashMap::new(),
    };
    let Some(bytes) = entry(archive, &rels_path) else {
        return out;
    };
    let root = xml::parse(&bytes);
    let mut found = Vec::new();
    root.find_all("Relationship", &mut found);
    for r in found {
        let (Some(id), Some(target)) = (r.attr("Id"), r.attr("Target")) else {
            continue;
        };
        let resolved = if r.attr("TargetMode") == Some("External") {
            target.to_string()
        } else {
            resolve_target(dir, target)
        };
        if let Some(kind) = r.attr("Type").and_then(|t| t.rsplit('/').next()) {
            out.by_type.insert(kind.to_string(), resolved.clone());
        }
        out.targets.insert(id.to_string(), resolved);
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Title,
    Subtitle,
    Body,
    Text,
    Skip,
}

fn role(shape: &Element) -> Role {
    let ph = shape.find("nvPr").and_then(|n| n.child("ph"));
    match ph {
        None => Role::Text,
        Some(ph) => match ph.attr("type") {
            Some("title") | Some("ctrTitle") => Role::Title,
            Some("subTitle") => Role::Subtitle,
            Some("dt") | Some("ftr") | Some("sldNum") | Some("hdr") | Some("sldImg") => Role::Skip,
            _ => Role::Body,
        },
    }
}

fn position(shape: &Element) -> (i64, i64) {
    let off = shape
        .child("spPr")
        .or_else(|| shape.child("grpSpPr"))
        .and_then(|p| p.path(&["xfrm", "off"]))
        .or_else(|| shape.path(&["xfrm", "off"]));
    let get = |k: &str| {
        off.and_then(|o| o.attr(k))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    (get("y"), get("x"))
}

fn paragraph_pieces(p: &Element, links: &HashMap<String, String>) -> Vec<Inline> {
    let mut pieces = Vec::new();
    for c in p.elements() {
        match c.local() {
            "r" | "fld" => {
                if c.is("fld") && c.attr("type") == Some("slidenum") {
                    continue;
                }
                let rpr = c.child("rPr");
                let flag =
                    |k: &str| matches!(rpr.and_then(|r| r.attr(k)), Some("1") | Some("true"));
                let format = Format {
                    bold: flag("b"),
                    italic: flag("i"),
                    strike: rpr
                        .and_then(|r| r.attr("strike"))
                        .is_some_and(|s| s != "noStrike"),
                    code: false,
                };
                let link = rpr
                    .and_then(|r| r.child("hlinkClick"))
                    .and_then(|h| h.attr_q("r:id").or_else(|| h.attr("id")))
                    .and_then(|id| links.get(id))
                    .filter(|t| t.contains(':'))
                    .cloned();
                let text = c.child("t").map(|t| t.text()).unwrap_or_default();
                pieces.push(Inline::text(text.replace('\t', " "), format, link));
            }
            "br" => pieces.push(Inline::text("\n", Format::default(), None)),
            _ => {}
        }
    }
    pieces
}

fn text_of(pieces: &[Inline]) -> String {
    pieces
        .iter()
        .map(|p| match p {
            Inline::Text { text, .. } => text.as_str(),
            Inline::Raw(_) => "",
        })
        .collect()
}

fn rendered(pieces: &[Inline]) -> String {
    render_inline(pieces)
        .split('\n')
        .map(|l| escape_line_start(l.trim()))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// A text body's paragraphs as blocks.
fn text_blocks(body: &Element, role: Role, links: &HashMap<String, String>) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut counters: Vec<u32> = Vec::new();
    for p in body.elements().filter(|e| e.is("p")) {
        let pieces = paragraph_pieces(p, links);
        if text_of(&pieces).trim().is_empty() {
            continue;
        }
        let text = rendered(&pieces);
        let ppr = p.child("pPr");
        let level = ppr
            .and_then(|x| x.attr("lvl"))
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        let explicit_none = ppr.and_then(|x| x.child("buNone")).is_some();
        let numbered = ppr.and_then(|x| x.child("buAutoNum")).is_some();
        let bullet = ppr
            .map(|x| x.child("buChar").is_some() || x.child("buAutoNum").is_some())
            .unwrap_or(false);
        let listed = !explicit_none && (bullet || role == Role::Body);
        if listed {
            if counters.len() <= level {
                counters.resize(level + 1, 0);
            }
            counters.truncate(level + 1);
            counters[level] += 1;
            blocks.push(Block::ListItem {
                level,
                number: numbered.then_some(counters[level]),
                text,
            });
        } else {
            counters.clear();
            blocks.push(Block::Paragraph(text));
        }
    }
    blocks
}

fn table_block(tbl: &Element, links: &HashMap<String, String>) -> Option<Block> {
    let mut rows = Vec::new();
    for tr in tbl.elements().filter(|e| e.is("tr")) {
        let mut cells = Vec::new();
        for tc in tr.elements().filter(|e| e.is("tc")) {
            let merged = matches!(tc.attr("hMerge"), Some("1") | Some("true"))
                || matches!(tc.attr("vMerge"), Some("1") | Some("true"));
            let text = if merged {
                String::new()
            } else {
                tc.child("txBody")
                    .map(|b| {
                        join_cell(
                            b.elements()
                                .filter(|e| e.is("p"))
                                .map(|p| rendered(&paragraph_pieces(p, links)))
                                .collect::<Vec<_>>(),
                        )
                    })
                    .unwrap_or_default()
            };
            cells.push(text);
        }
        rows.push(cells);
    }
    (!rows.iter().flatten().all(|c| c.trim().is_empty())).then_some(Block::Table(rows))
}

/// Shapes in a tree, each as (role, position, blocks); groups flatten in
/// place at their own position.
fn shapes(
    tree: &Element,
    links: &HashMap<String, String>,
    out: &mut Vec<(Role, (i64, i64), Vec<Block>)>,
) {
    for shape in tree.elements() {
        match shape.local() {
            "sp" => {
                let r = role(shape);
                if r == Role::Skip {
                    continue;
                }
                if let Some(body) = shape.child("txBody") {
                    let blocks = text_blocks(body, r, links);
                    if !blocks.is_empty() {
                        out.push((r, position(shape), blocks));
                    }
                }
            }
            "grpSp" => {
                let mut inner = Vec::new();
                shapes(shape, links, &mut inner);
                inner.sort_by_key(|(_, pos, _)| *pos);
                let pos = position(shape);
                for (r, _, b) in inner {
                    out.push((r, pos, b));
                }
            }
            "graphicFrame" => {
                if let Some(tbl) = shape.find("tbl") {
                    if let Some(b) = table_block(tbl, links) {
                        out.push((Role::Text, position(shape), vec![b]));
                    }
                }
            }
            "pic" => {
                let desc = shape
                    .find("cNvPr")
                    .and_then(|c| c.attr("descr"))
                    .filter(|d| !d.trim().is_empty());
                if let Some(d) = desc {
                    out.push((
                        Role::Text,
                        position(shape),
                        vec![Block::Paragraph(render_inline(&[image_placeholder(Some(
                            d,
                        ))]))],
                    ));
                }
            }
            "AlternateContent" => {
                if let Some(choice) = shape.child("Choice") {
                    shapes(choice, links, out);
                }
            }
            _ => {}
        }
    }
}

fn block_text(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            Block::Paragraph(t) | Block::ListItem { text: t, .. } => Some(t.replace('\n', " ")),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn slide_markdown(archive: &mut Archive, part: &str) -> String {
    let Some(bytes) = entry(archive, part) else {
        return String::new();
    };
    let slide_rels = rels(archive, part);
    let root = xml::parse(&bytes);
    let mut found = Vec::new();
    if let Some(tree) = root.find("spTree") {
        shapes(tree, &slide_rels.targets, &mut found);
    }
    let mut blocks: Vec<Block> = Vec::new();
    if let Some(i) = found.iter().position(|(r, _, _)| *r == Role::Title) {
        let (_, _, title) = found.remove(i);
        let text = block_text(&title);
        if !text.trim().is_empty() {
            blocks.push(Block::Heading { level: 2, text });
        }
    }
    found.sort_by_key(|(_, pos, _)| *pos);
    for (_, _, b) in found {
        blocks.extend(b);
    }

    if let Some(notes_part) = slide_rels.by_type.get("notesSlide").cloned() {
        if let Some(nbytes) = entry(archive, &notes_part) {
            let nroot = xml::parse(&nbytes);
            let mut notes = Vec::new();
            if let Some(tree) = nroot.find("spTree") {
                for shape in tree.elements().filter(|e| e.is("sp")) {
                    let is_notes = shape
                        .find("nvPr")
                        .and_then(|n| n.child("ph"))
                        .is_some_and(|ph| ph.attr("type") == Some("body"));
                    if let (true, Some(body)) = (is_notes, shape.child("txBody")) {
                        for b in text_blocks(body, Role::Text, &slide_rels.targets) {
                            notes.push(b);
                        }
                    }
                }
            }
            if !notes.is_empty() {
                blocks.push(Block::Paragraph("**Speaker notes**".to_string()));
                blocks.extend(notes);
            }
        }
    }
    render_blocks(&blocks)
}

/// Convert a `.pptx` to markdown, one entry per slide.
pub fn pptx_to_markdown(path: &Path) -> Result<PptxMarkdown, String> {
    let mut archive = open(path)?;
    let pres = entry(&mut archive, "ppt/presentation.xml")
        .ok_or_else(|| "missing ppt/presentation.xml".to_string())?;
    let pres_rels = rels(&mut archive, "ppt/presentation.xml");
    let root = xml::parse(&pres);
    let mut ids = Vec::new();
    if let Some(list) = root.find("sldIdLst") {
        for s in list.elements().filter(|e| e.is("sldId")) {
            if let Some(rid) = s.attr_q("r:id").or_else(|| s.attr("id")) {
                if let Some(part) = pres_rels.targets.get(rid) {
                    ids.push(part.clone());
                }
            }
        }
    }
    let mut slides = Vec::new();
    for (i, part) in ids.iter().enumerate() {
        slides.push((i as u32 + 1, slide_markdown(&mut archive, part)));
    }
    let title = entry(&mut archive, "docProps/core.xml").and_then(|x| meta_title(&x));
    Ok(PptxMarkdown { slides, title })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slide(sp_tree: &str) -> Element {
        xml::parse(
            format!(r#"<p:sld xmlns:p="p" xmlns:a="a"><p:cSld><p:spTree>{sp_tree}</p:spTree></p:cSld></p:sld>"#)
                .as_bytes(),
        )
    }

    fn sp(ph: &str, y: i64, paras: &str) -> String {
        format!(
            r#"<p:sp><p:nvSpPr><p:nvPr>{ph}</p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x="0" y="{y}"/></a:xfrm></p:spPr><p:txBody>{paras}</p:txBody></p:sp>"#
        )
    }

    fn render(root: &Element) -> String {
        let mut found = Vec::new();
        shapes(root.find("spTree").unwrap(), &HashMap::new(), &mut found);
        let mut blocks = Vec::new();
        if let Some(i) = found.iter().position(|(r, _, _)| *r == Role::Title) {
            let (_, _, t) = found.remove(i);
            blocks.push(Block::Heading {
                level: 2,
                text: block_text(&t),
            });
        }
        found.sort_by_key(|(_, pos, _)| *pos);
        for (_, _, b) in found {
            blocks.extend(b);
        }
        render_blocks(&blocks)
    }

    #[test]
    fn title_then_body_bullets_by_level() {
        let root = slide(&[
            sp(
                r#"<p:ph type="body" idx="1"/>"#,
                500,
                r#"<a:p><a:r><a:t>Tale types</a:t></a:r></a:p><a:p><a:pPr lvl="1"/><a:r><a:rPr b="1"/><a:t>ATU</a:t></a:r></a:p>"#,
            ),
            sp(
                r#"<p:ph type="title"/>"#,
                100,
                r#"<a:p><a:r><a:t>Folktales</a:t></a:r></a:p>"#,
            ),
            sp(r#"<p:ph type="sldNum"/>"#, 900, r#"<a:p><a:r><a:t>3</a:t></a:r></a:p>"#),
        ]
        .concat());
        assert_eq!(render(&root), "## Folktales\n\n- Tale types\n  - **ATU**");
    }

    #[test]
    fn plain_text_boxes_are_paragraphs() {
        let root = slide(&sp("", 0, r#"<a:p><a:r><a:t>Just text</a:t></a:r></a:p>"#));
        assert_eq!(render(&root), "Just text");
    }
}
