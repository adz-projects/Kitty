//! Hand-rolled `.docx` writer: `zip` + literal OOXML string assembly, no
//! docx-rs. Per the base plan's rationale: list styles ("List Bullet"/"List
//! Number") and the WCAG accessibility structures (`w:tblHeader`, `w:lang`,
//! `dc:title`) aren't reachable through docx-rs's public API, and hand-rolled
//! **append** can byte-copy every part it doesn't touch — strictly higher
//! fidelity than reading, mutating, and re-serializing a typed document
//! model would ever be.
//!
//! Static parts are lifted once as `include_str!` assets under `assets/` —
//! kept as the emitted, final XML (not a Rust string literal someone
//! hand-transcribed), so there is exactly one place a byte can drift from
//! what Word actually expects.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::path::Path;

use regex::Regex;
use zip::write::SimpleFileOptions;
use zip::ZipArchive;

use super::DocxError;

const CONTENT_TYPES: &str = include_str!("assets/content_types.xml");
const ROOT_RELS: &str = include_str!("assets/root_rels.xml");
const DOCUMENT_RELS: &str = include_str!("assets/document_rels.xml");
const NUMBERING: &str = include_str!("assets/numbering.xml");
const STYLES_TEMPLATE: &str = include_str!("assets/styles.xml");
const CORE_TEMPLATE: &str = include_str!("assets/core.xml");
const APP_PROPS: &str = include_str!("assets/app.xml");

const DOCUMENT_XML_NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006""#;

pub enum WriteMode {
    Create,
    Append,
}

pub struct WriteResult {
    pub path: String,
    pub mode: &'static str,
    pub language: String,
}

pub fn write_document(
    path: &Path,
    doc_text: Option<&str>,
    mode: WriteMode,
    title: Option<&str>,
    language: &str,
) -> Result<WriteResult, DocxError> {
    match mode {
        WriteMode::Create => create(path, doc_text, title, language),
        WriteMode::Append => append(path, doc_text, title, language),
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

// ---------------------------------------------------------------------------
// Markdown-lite -> OOXML body renderer (mirrors lean_mcp.py's word_write_doc
// line-by-line dispatch exactly, including its edge behaviors)
// ---------------------------------------------------------------------------

/// Hyperlinks discovered while rendering a body, in the order they appear.
///
/// A `w:hyperlink` does not carry its URL: it carries an `r:id` pointing at a
/// relationship in `word/_rels/document.xml.rels`, so the target lives in a
/// different part of the package from the text that links to it. Rendering
/// therefore has to *collect* as it goes and hand the caller the relationship
/// entries to write alongside the document.
///
/// Ids start above whatever the package already uses — `create` seeds from the
/// static relationships in `document_rels.xml`, `append` from the highest
/// `rIdN` already in the file — because reusing an id would silently repoint
/// an existing relationship (styles, numbering, an image) at a URL.
#[derive(Debug, Default)]
pub(crate) struct LinkCollector {
    next_id: u32,
    links: Vec<(String, String)>,
}

impl LinkCollector {
    fn starting_at(next_id: u32) -> Self {
        Self {
            next_id,
            links: Vec::new(),
        }
    }

    fn add(&mut self, url: &str) -> String {
        let id = format!("rId{}", self.next_id);
        self.next_id += 1;
        self.links.push((id.clone(), url.to_string()));
        id
    }

    fn is_empty(&self) -> bool {
        self.links.is_empty()
    }

    /// The `<Relationship>` elements to splice into `document.xml.rels`.
    fn relationships_xml(&self) -> String {
        self.links
            .iter()
            .map(|(id, url)| {
                format!(
                    r#"<Relationship Id="{id}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="{}" TargetMode="External"/>"#,
                    xml_escape(url)
                )
            })
            .collect()
    }
}

/// Whether a markdown link target is safe to write into a document.
///
/// The text being rendered came from a model, and the `.docx` is something the
/// user opens later in Word — so a `javascript:`/`vbscript:` URL, or a `file:`
/// target pointing at a UNC path, is a link the document should simply not
/// contain. Word's own prompts are not a reason to emit one. An unrecognised
/// scheme falls back to plain text with the URL still visible, which is a
/// strictly better failure than a live link to somewhere unexpected.
fn is_safe_link_target(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    ["http://", "https://", "mailto:"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
}

fn render_inline_runs(text: &str, links: &mut LinkCollector) -> String {
    // Markdown links first in the alternation, so `[a **b**](url)` is seen as a
    // link rather than having its label chewed up by the emphasis patterns. The
    // target stops at whitespace or `)`, which keeps `see [docs](https://x) for
    // more` from swallowing the rest of the line.
    static PATTERN: &str = r"(\[[^\]]*\]\([^)\s]*\)|\*\*.*?\*\*|\*.*?\*)";
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(PATTERN).unwrap());

    let mut out = String::new();
    let mut last = 0;
    for m in re.find_iter(text) {
        if m.start() > last {
            out.push_str(&plain_run(&text[last..m.start()]));
        }
        let token = m.as_str();
        if let Some((label, url)) = parse_markdown_link(token) {
            out.push_str(&link_or_plain(label, url, links));
        } else if token.starts_with("**") && token.ends_with("**") && token.len() >= 4 {
            out.push_str(&run(&token[2..token.len() - 2], true, false, None));
        } else if token.starts_with('*') && token.ends_with('*') && token.len() >= 2 {
            out.push_str(&run(&token[1..token.len() - 1], false, true, None));
        } else {
            out.push_str(&plain_run(token));
        }
        last = m.end();
    }
    if last < text.len() {
        out.push_str(&plain_run(&text[last..]));
    }
    out
}

/// `[label](url)` -> `(label, url)`. `None` for anything else the emphasis
/// alternation matched.
fn parse_markdown_link(token: &str) -> Option<(&str, &str)> {
    let rest = token.strip_prefix('[')?;
    let close = rest.find("](")?;
    let label = &rest[..close];
    let url = rest[close + 2..].strip_suffix(')')?;
    Some((label, url))
}

/// A real hyperlink when the target is one we are willing to write, otherwise
/// the markdown rendered as ordinary text.
fn link_or_plain(label: &str, url: &str, links: &mut LinkCollector) -> String {
    if !is_safe_link_target(url) {
        // Keep the URL visible rather than dropping it: the reader can still
        // see where it was meant to point and judge it themselves.
        return plain_run(&format!("{label} ({url})"));
    }
    let rel_id = links.add(url.trim());
    // An empty label would render as an invisible, unclickable link; show the
    // target instead, which is what a bare URL in prose looks like anyway.
    let shown = if label.trim().is_empty() { url } else { label };
    format!(
        r#"<w:hyperlink r:id="{rel_id}">{}</w:hyperlink>"#,
        run(shown, false, false, Some("Hyperlink"))
    )
}

fn plain_run(text: &str) -> String {
    run(text, false, false, None)
}

fn run(text: &str, bold: bool, italic: bool, style: Option<&str>) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut rpr = String::new();
    if bold || italic || style.is_some() {
        rpr.push_str("<w:rPr>");
        // `w:rStyle` comes first in `w:rPr`: the schema's sequence is ordered
        // and Word rejects a document that gets it wrong.
        if let Some(style) = style {
            rpr.push_str(&format!(r#"<w:rStyle w:val="{style}"/>"#));
        }
        if bold {
            rpr.push_str("<w:b/>");
        }
        if italic {
            rpr.push_str("<w:i/>");
        }
        rpr.push_str("</w:rPr>");
    }
    format!(
        r#"<w:r>{rpr}<w:t xml:space="preserve">{}</w:t></w:r>"#,
        xml_escape(text)
    )
}

fn paragraph_xml(style_id: Option<&str>, extra_ppr: &str, runs_xml: &str) -> String {
    let style = style_id
        .map(|s| format!(r#"<w:pStyle w:val="{s}"/>"#))
        .unwrap_or_default();
    if style.is_empty() && extra_ppr.is_empty() {
        format!("<w:p>{runs_xml}</w:p>")
    } else {
        format!("<w:p><w:pPr>{style}{extra_ppr}</w:pPr>{runs_xml}</w:p>")
    }
}

fn heading_xml(level: u32, text: &str, links: &mut LinkCollector) -> String {
    // level 0 = "Title" style (python-docx's `add_heading(title, level=0)`
    // convention, reproduced literally — the base plan notes `create` mode
    // always emits this first).
    let style = if level == 0 {
        "Title".to_string()
    } else {
        format!("Heading{}", level.min(4))
    };
    paragraph_xml(Some(&style), "", &render_inline_runs(text, links))
}

fn list_paragraph_xml(
    style_id: &str,
    num_id: u32,
    text: &str,
    links: &mut LinkCollector,
) -> String {
    let num_pr = format!(r#"<w:numPr><w:ilvl w:val="0"/><w:numId w:val="{num_id}"/></w:numPr>"#);
    paragraph_xml(Some(style_id), &num_pr, &render_inline_runs(text, links))
}

/// A markdown-lite table block: consumes lines only while they both start
/// and end with `|`. The separator row (`^\|[\s\-:\t|]+\|$`) is skipped, not
/// rendered as data. `num_cols = max(len(row))` so short rows leave
/// trailing cells empty. Row 0 gets `w:tblHeader`; **every** row (row 0
/// included) gets `w:cantSplit` — both ported literally from
/// `_make_table_accessible`, which is exactly what it did.
fn table_xml(rows: &[Vec<String>], links: &mut LinkCollector) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let num_cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    if num_cols == 0 {
        return String::new();
    }

    let grid_cols: String = (0..num_cols)
        .map(|_| r#"<w:gridCol w:w="2000"/>"#)
        .collect();

    let mut trs = String::new();
    for (r_idx, row) in rows.iter().enumerate() {
        let mut tc_pr_header = String::new();
        if r_idx == 0 {
            tc_pr_header.push_str("<w:tblHeader/>");
        }
        tc_pr_header.push_str("<w:cantSplit/>");

        let mut cells = String::new();
        for c_idx in 0..num_cols {
            let cell_text = row.get(c_idx).map(String::as_str).unwrap_or("");
            cells.push_str(&format!(
                "<w:tc><w:tcPr><w:tcW w:w=\"2000\" w:type=\"dxa\"/></w:tcPr>{}</w:tc>",
                paragraph_xml(None, "", &render_inline_runs(cell_text, links))
            ));
        }
        trs.push_str(&format!(
            "<w:tr><w:trPr>{tc_pr_header}</w:trPr>{cells}</w:tr>"
        ));
    }

    format!(
        r#"<w:tbl><w:tblPr><w:tblStyle w:val="TableGrid"/><w:tblW w:w="0" w:type="auto"/><w:tblBorders>
            <w:top w:val="single" w:sz="4" w:space="0" w:color="auto"/>
            <w:left w:val="single" w:sz="4" w:space="0" w:color="auto"/>
            <w:bottom w:val="single" w:sz="4" w:space="0" w:color="auto"/>
            <w:right w:val="single" w:sz="4" w:space="0" w:color="auto"/>
            <w:insideH w:val="single" w:sz="4" w:space="0" w:color="auto"/>
            <w:insideV w:val="single" w:sz="4" w:space="0" w:color="auto"/>
        </w:tblBorders></w:tblPr><w:tblGrid>{grid_cols}</w:tblGrid>{trs}</w:tbl>"#
    )
}

/// First relationship id `create` may hand out: `document_rels.xml` ships
/// rId1 (styles) and rId2 (numbering).
const FIRST_FREE_REL_ID: u32 = 3;

/// The `Hyperlink` character style, as Word itself writes it — blue and
/// underlined, which is what makes a link look like one.
const HYPERLINK_STYLE: &str = r#"<w:style w:type="character" w:styleId="Hyperlink"><w:name w:val="Hyperlink"/><w:basedOn w:val="DefaultParagraphFont"/><w:rPr><w:color w:val="0563C1"/><w:u w:val="single"/></w:rPr></w:style>"#;

/// Lowest `rIdN` not already taken by `rels_xml`.
///
/// Scans for the numeric suffix of every `Id="rIdN"` rather than counting
/// elements: ids in a real document are not necessarily contiguous or ordered
/// (Word leaves gaps when a relationship is deleted), so "one past the count"
/// would happily collide.
fn next_free_rel_id(rels_xml: &str) -> u32 {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"Id="rId(\d+)""#).unwrap());
    let highest = re
        .captures_iter(rels_xml)
        .filter_map(|c| c.get(1)?.as_str().parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    highest.saturating_add(1).max(FIRST_FREE_REL_ID)
}

/// Splices `links`' relationship elements in before the closing tag.
///
/// A no-op when there are no links, so a document with none is written
/// byte-identically to before this feature existed.
fn insert_relationships(rels_xml: &str, links: &LinkCollector) -> String {
    if links.is_empty() {
        return rels_xml.to_string();
    }
    let additions = links.relationships_xml();
    match rels_xml.rfind("</Relationships>") {
        Some(idx) => format!("{}{additions}{}", &rels_xml[..idx], &rels_xml[idx..]),
        // No closing tag means this isn't a relationships part we understand;
        // returning it untouched loses the links but cannot corrupt the file.
        None => rels_xml.to_string(),
    }
}

/// Declares the relationships namespace on `<w:document>` if it isn't already.
fn ensure_relationship_namespace(document_xml: &str) -> String {
    if document_xml.contains("xmlns:r=") {
        return document_xml.to_string();
    }
    match document_xml.find("<w:document") {
        Some(start) => {
            let insert_at = start + "<w:document".len();
            format!(
                r#"{} xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"{}"#,
                &document_xml[..insert_at],
                &document_xml[insert_at..]
            )
        }
        None => document_xml.to_string(),
    }
}

/// Adds the `Hyperlink` character style if the package doesn't define one.
fn ensure_hyperlink_style(styles_xml: &str) -> String {
    if styles_xml.contains(r#"w:styleId="Hyperlink""#) {
        return styles_xml.to_string();
    }
    match styles_xml.rfind("</w:styles>") {
        Some(idx) => format!(
            "{}{HYPERLINK_STYLE}{}",
            &styles_xml[..idx],
            &styles_xml[idx..]
        ),
        None => styles_xml.to_string(),
    }
}

fn is_table_separator(line: &str) -> bool {
    static SEP: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = SEP.get_or_init(|| Regex::new(r"^\|[\s\-:\t|]+\|$").unwrap());
    re.is_match(line)
}

/// Renders `doc_text` into a sequence of body-XML fragments, mirroring
/// `word_write_doc`'s line dispatch loop exactly (table detection,
/// heading levels longest-prefix-first, bullet/number lists, else Normal).
fn render_body(doc_text: &str, links: &mut LinkCollector) -> String {
    let lines = crate::text::py_splitlines(doc_text.trim());
    let mut out = String::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        if line.is_empty() {
            i += 1;
            continue;
        }

        if line.starts_with('|') && line.ends_with('|') {
            let mut table_lines = Vec::new();
            while i < lines.len() {
                let t = lines[i].trim();
                if t.starts_with('|') && t.ends_with('|') {
                    table_lines.push(t.to_string());
                    i += 1;
                } else {
                    break;
                }
            }
            let rows: Vec<Vec<String>> = table_lines
                .iter()
                .filter(|l| !is_table_separator(l))
                .map(|l| {
                    // A bare "|" line passes the starts/ends-with-`|` gate
                    // above but has no interior: `&l[1..l.len() - 1]` would
                    // slice `[1..0]` and panic (audit #114). Python's
                    // forgiving `line[1:-1]` yields "" here, which splits to
                    // a single empty cell — mirrored.
                    let inner = if l.len() >= 2 { &l[1..l.len() - 1] } else { "" };
                    inner.split('|').map(|c| c.trim().to_string()).collect()
                })
                .collect();
            if !rows.is_empty() {
                out.push_str(&table_xml(&rows, links));
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix("#### ") {
            out.push_str(&heading_xml(4, rest, links));
        } else if let Some(rest) = line.strip_prefix("### ") {
            out.push_str(&heading_xml(3, rest, links));
        } else if let Some(rest) = line.strip_prefix("## ") {
            out.push_str(&heading_xml(2, rest, links));
        } else if let Some(rest) = line.strip_prefix("# ") {
            out.push_str(&heading_xml(1, rest, links));
        } else if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            out.push_str(&list_paragraph_xml("ListBullet", 1, rest, links));
        } else if let Some(rest) = strip_ordered_list_prefix(line) {
            out.push_str(&list_paragraph_xml("ListNumber", 2, rest, links));
        } else {
            out.push_str(&paragraph_xml(
                Some("Normal"),
                "",
                &render_inline_runs(line, links),
            ));
        }
        i += 1;
    }
    out
}

fn strip_ordered_list_prefix(line: &str) -> Option<&str> {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^\d+\.\s").unwrap());
    re.find(line).map(|m| &line[m.end()..])
}

// ---------------------------------------------------------------------------
// Create mode
// ---------------------------------------------------------------------------

fn create(
    path: &Path,
    doc_text: Option<&str>,
    title: Option<&str>,
    language: &str,
) -> Result<WriteResult, DocxError> {
    let doc_title = title.map(str::to_string).unwrap_or_else(|| {
        path.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    });

    // Seeded past the two static relationships in `document_rels.xml`
    // (styles = rId1, numbering = rId2) so a link can never collide with one.
    let mut links = LinkCollector::starting_at(FIRST_FREE_REL_ID);
    let mut body = heading_xml(0, &doc_title, &mut links);
    if let Some(text) = doc_text {
        if !text.trim().is_empty() {
            body.push_str(&render_body(text, &mut links));
        }
    }

    let document_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document {DOCUMENT_XML_NS}><w:body>{body}<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440"/></w:sectPr></w:body></w:document>"#
    );

    // WCAG accessibility metadata (Track: reproduce, not fix, the schema
    // violation — see `_set_doc_accessibility_meta`'s doc comment below).
    let styles_xml = append_lang_to_styles_root(STYLES_TEMPLATE, language);
    // Single-pass token substitution (shared with the viz HTML wrapper) — the
    // old `.replace("__TITLE__", t).replace("__LANGUAGE__", l)` chain let a
    // title containing the literal `__LANGUAGE__` get clobbered by the
    // language step; render_template substitutes each token exactly once and
    // never re-scans substituted values.
    let core_xml = crate::tools::viz::escape::render_template(
        CORE_TEMPLATE,
        &[
            ("TITLE", &xml_escape(&doc_title)),
            ("LANGUAGE", &xml_escape(language)),
        ],
    );

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| DocxError::Corrupt(e.to_string()))?;
        }
    }
    let file = std::fs::File::create(path).map_err(|e| DocxError::Corrupt(e.to_string()))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    write_zip_str(&mut zip, "[Content_Types].xml", CONTENT_TYPES, opts)?;
    write_zip_str(&mut zip, "_rels/.rels", ROOT_RELS, opts)?;
    write_zip_str(
        &mut zip,
        "word/_rels/document.xml.rels",
        &insert_relationships(DOCUMENT_RELS, &links),
        opts,
    )?;
    write_zip_str(&mut zip, "word/document.xml", &document_xml, opts)?;
    write_zip_str(&mut zip, "word/styles.xml", &styles_xml, opts)?;
    write_zip_str(&mut zip, "word/numbering.xml", NUMBERING, opts)?;
    write_zip_str(&mut zip, "docProps/core.xml", &core_xml, opts)?;
    write_zip_str(&mut zip, "docProps/app.xml", APP_PROPS, opts)?;
    zip.finish()
        .map_err(|e| DocxError::Corrupt(e.to_string()))?;

    Ok(WriteResult {
        path: path.to_string_lossy().to_string(),
        mode: "create",
        language: language.to_string(),
    })
}

fn write_zip_str(
    zip: &mut zip::ZipWriter<std::fs::File>,
    name: &str,
    contents: &str,
    opts: SimpleFileOptions,
) -> Result<(), DocxError> {
    zip.start_file(name, opts)
        .map_err(|e| DocxError::Corrupt(e.to_string()))?;
    zip.write_all(contents.as_bytes())
        .map_err(|e| DocxError::Corrupt(e.to_string()))?;
    Ok(())
}

/// Appends `<w:lang w:val="...">` as a **direct child of the `<w:styles>`
/// root element** — not schema-valid (`w:lang` belongs inside
/// `w:docDefaults/w:rPrDefault/w:rPr`), but this is a deliberate,
/// documented reproduction of `_set_doc_accessibility_meta`'s exact
/// behavior (Word tolerates it; the base plan calls for reproducing this
/// bug-for-bug and filing a follow-up rather than silently fixing it here).
fn append_lang_to_styles_root(styles_xml: &str, language: &str) -> String {
    let lang_element = format!(r#"<w:lang w:val="{}"/>"#, xml_escape(language));
    match styles_xml.rfind("</w:styles>") {
        Some(idx) => {
            let mut out = String::with_capacity(styles_xml.len() + lang_element.len());
            out.push_str(&styles_xml[..idx]);
            out.push_str(&lang_element);
            out.push_str(&styles_xml[idx..]);
            out
        }
        None => styles_xml.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Append mode — byte-copy every part except document.xml (always) and
// styles.xml/docProps/core.xml (only if the accessibility metadata step
// actually touches them), splicing new paragraphs before the existing
// `<w:sectPr>`.
// ---------------------------------------------------------------------------

fn append(
    path: &Path,
    doc_text: Option<&str>,
    title: Option<&str>,
    language: &str,
) -> Result<WriteResult, DocxError> {
    if !path.exists() {
        return Err(DocxError::NotFound);
    }
    let read_file = std::fs::File::open(path).map_err(|e| DocxError::Corrupt(e.to_string()))?;
    let mut archive = ZipArchive::new(read_file).map_err(|e| DocxError::Corrupt(e.to_string()))?;
    if archive.len() > super::MAX_DOCX_ENTRIES {
        return Err(DocxError::Corrupt(format!(
            "archive has too many entries ({})",
            archive.len()
        )));
    }

    let mut parts: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| DocxError::Corrupt(e.to_string()))?;
        let name = entry.name().to_string();
        let mut buf = Vec::new();
        // Cap decompression per part (zip-bomb hardening, same as the read
        // path) so a single bombed entry can't exhaust memory on append.
        entry
            .take(super::MAX_DOCX_ENTRY_BYTES + 1)
            .read_to_end(&mut buf)
            .map_err(|e| DocxError::Corrupt(e.to_string()))?;
        if buf.len() as u64 > super::MAX_DOCX_ENTRY_BYTES {
            return Err(DocxError::Corrupt(format!(
                "part {name} exceeds the {} byte decompressed-size cap",
                super::MAX_DOCX_ENTRY_BYTES
            )));
        }
        parts.insert(name, buf);
    }
    drop(archive);

    let document_xml = parts
        .get("word/document.xml")
        .ok_or_else(|| DocxError::Corrupt("missing word/document.xml".to_string()))?;
    let document_xml = String::from_utf8_lossy(document_xml).to_string();

    // Relationship ids have to clear everything the existing package already
    // uses — including parts this writer never emits (images, headers, an
    // embedded font). Reusing one would repoint that relationship at a URL.
    let rels_xml = parts
        .get("word/_rels/document.xml.rels")
        .map(|b| String::from_utf8_lossy(b).to_string())
        .unwrap_or_else(|| DOCUMENT_RELS.to_string());
    let mut links = LinkCollector::starting_at(next_free_rel_id(&rels_xml));

    let mut new_body = String::new();
    if let Some(text) = doc_text {
        if !text.trim().is_empty() {
            new_body = render_body(text, &mut links);
        }
    }

    let spliced = splice_before_sect_pr(&document_xml, &new_body);
    // A document written by an older build of this writer has no `xmlns:r` on
    // its root, and `r:id` against an undeclared prefix is not well-formed XML
    // — Word refuses to open the file at all. Only touched when links were
    // actually emitted, so an append with no links leaves the root byte-identical.
    let spliced = if links.is_empty() {
        spliced
    } else {
        ensure_relationship_namespace(&spliced)
    };
    parts.insert("word/document.xml".to_string(), spliced.into_bytes());

    if !links.is_empty() {
        parts.insert(
            "word/_rels/document.xml.rels".to_string(),
            insert_relationships(&rels_xml, &links).into_bytes(),
        );
    }

    // `_set_doc_accessibility_meta` runs on append too — the language
    // append hits styles.xml regardless of `title`; the title change to
    // core.xml is conditional on `title` being given, matching
    // `if title: doc.core_properties.title = title`.
    if let Some(styles_bytes) = parts.get("word/styles.xml").cloned() {
        let styles_str = String::from_utf8_lossy(&styles_bytes).to_string();
        let patched = append_lang_to_styles_root(&styles_str, language);
        // A `w:rStyle` naming a style the package doesn't define is legal but
        // renders as body text — the link would work and look like nothing.
        let patched = if links.is_empty() {
            patched
        } else {
            ensure_hyperlink_style(&patched)
        };
        parts.insert("word/styles.xml".to_string(), patched.into_bytes());
    }
    if let Some(t) = title {
        if let Some(core_bytes) = parts.get("docProps/core.xml").cloned() {
            let core_str = String::from_utf8_lossy(&core_bytes).to_string();
            let patched = set_or_insert_title(&core_str, t);
            parts.insert("docProps/core.xml".to_string(), patched.into_bytes());
        }
    }

    let file = std::fs::File::create(path).map_err(|e| DocxError::Corrupt(e.to_string()))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in &parts {
        zip.start_file(name, opts)
            .map_err(|e| DocxError::Corrupt(e.to_string()))?;
        zip.write_all(bytes)
            .map_err(|e| DocxError::Corrupt(e.to_string()))?;
    }
    zip.finish()
        .map_err(|e| DocxError::Corrupt(e.to_string()))?;

    Ok(WriteResult {
        path: path.to_string_lossy().to_string(),
        mode: "append",
        language: language.to_string(),
    })
}

fn splice_before_sect_pr(document_xml: &str, new_body_xml: &str) -> String {
    if new_body_xml.is_empty() {
        return document_xml.to_string();
    }
    if let Some(idx) = document_xml.find("<w:sectPr") {
        let mut out = String::with_capacity(document_xml.len() + new_body_xml.len());
        out.push_str(&document_xml[..idx]);
        out.push_str(new_body_xml);
        out.push_str(&document_xml[idx..]);
        return out;
    }
    if let Some(idx) = document_xml.rfind("</w:body>") {
        let mut out = String::with_capacity(document_xml.len() + new_body_xml.len());
        out.push_str(&document_xml[..idx]);
        out.push_str(new_body_xml);
        out.push_str(&document_xml[idx..]);
        return out;
    }
    document_xml.to_string()
}

fn set_or_insert_title(core_xml: &str, title: &str) -> String {
    let escaped = xml_escape(title);
    static TITLE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = TITLE_RE.get_or_init(|| Regex::new(r"(?s)<dc:title>.*?</dc:title>").unwrap());
    if re.is_match(core_xml) {
        // `NoExpand`, not a plain replacement string: `Regex::replace`
        // `$`-expands its replacement, so a title containing `$` (e.g.
        // "Price is $5") would be mangled into capture-group references
        // (audit #115).
        let replacement = format!("<dc:title>{escaped}</dc:title>");
        re.replace(core_xml, regex::NoExpand(replacement.as_str()))
            .to_string()
    } else if let Some(idx) = core_xml.rfind("</cp:coreProperties>") {
        let mut out = String::with_capacity(core_xml.len() + escaped.len() + 32);
        out.push_str(&core_xml[..idx]);
        out.push_str(&format!("<dc:title>{escaped}</dc:title>"));
        out.push_str(&core_xml[idx..]);
        out
    } else {
        core_xml.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_inline_runs_handles_bold_then_italic_non_greedy() {
        let xml = render_inline_runs("**bold** and *italic* text", &mut LinkCollector::default());
        assert!(xml.contains("<w:b/>"));
        assert!(xml.contains("<w:i/>"));
        assert!(xml.contains(">bold<"));
        assert!(xml.contains(">italic<"));
    }

    #[test]
    fn lone_asterisk_is_literal() {
        let xml = render_inline_runs("a * b", &mut LinkCollector::default());
        assert!(!xml.contains("<w:b/>"));
        assert!(!xml.contains("<w:i/>"));
    }

    #[test]
    fn heading_dispatch_prefers_longest_prefix_first() {
        let body = render_body(
            "#### four\n### three\n## two\n# one",
            &mut LinkCollector::default(),
        );
        // Ensure each maps to its own distinct heading style, not misfired
        // by a shorter prefix matching first (e.g. "#" matching "####").
        assert!(body.contains(r#"w:val="Heading4""#));
        assert!(body.contains(r#"w:val="Heading3""#));
        assert!(body.contains(r#"w:val="Heading2""#));
        assert!(body.contains(r#"w:val="Heading1""#));
    }

    #[test]
    fn table_rows_short_row_leaves_trailing_cells_empty() {
        let rows = vec![
            vec!["a".to_string(), "b".to_string()],
            vec!["c".to_string()],
        ];
        let xml = table_xml(&rows, &mut LinkCollector::default());
        // 2 columns (max row len), second row's second cell empty.
        assert_eq!(xml.matches("<w:gridCol").count(), 2);
    }

    #[test]
    fn table_header_and_cant_split_on_every_row() {
        let rows = vec![vec!["h".to_string()], vec!["v".to_string()]];
        let xml = table_xml(&rows, &mut LinkCollector::default());
        assert_eq!(xml.matches("<w:tblHeader/>").count(), 1);
        assert_eq!(xml.matches("<w:cantSplit/>").count(), 2);
    }

    #[test]
    fn table_separator_row_is_not_rendered_as_data() {
        let body = render_body(
            "| a | b |\n|---|---|\n| 1 | 2 |",
            &mut LinkCollector::default(),
        );
        assert!(body.contains(">a<"));
        assert!(body.contains(">1<"));
        assert!(!body.contains("---"));
    }

    #[test]
    fn bare_pipe_line_does_not_panic() {
        // Audit #114: a lone "|" passed the table gate and panicked slicing
        // `[1..0]`. It now renders as one empty cell, mirroring Python's
        // forgiving `line[1:-1]`.
        let body = render_body("|", &mut LinkCollector::default());
        assert!(
            body.contains("<w:tbl>"),
            "a pipe-gated line still renders as a table: {body}"
        );
    }

    #[test]
    fn title_with_dollar_signs_is_not_mangled_by_regex_replacement() {
        // Audit #115: `Regex::replace` `$`-expands the replacement string, so
        // "Price is $5" lost the `$5` (a bogus capture reference). NoExpand
        // inserts the title verbatim.
        let core = "<cp:coreProperties><dc:title>old</dc:title></cp:coreProperties>";
        let out = set_or_insert_title(core, "Price is $5 and $1 and $$ and ${name}");
        assert!(
            out.contains("<dc:title>Price is $5 and $1 and $$ and ${name}</dc:title>"),
            "title must survive verbatim: {out}"
        );
    }

    #[test]
    fn append_lang_lands_as_direct_child_of_styles_root() {
        let styles = r#"<w:styles xmlns:w="ns"><w:style/></w:styles>"#;
        let patched = append_lang_to_styles_root(styles, "en-US");
        assert!(patched.ends_with(r#"<w:lang w:val="en-US"/></w:styles>"#));
    }

    #[test]
    fn splice_inserts_before_sect_pr() {
        let doc = r#"<w:document><w:body><w:p/><w:sectPr/></w:body></w:document>"#;
        let spliced = splice_before_sect_pr(doc, "<w:p>NEW</w:p>");
        let sect_idx = spliced.find("<w:sectPr").unwrap();
        let new_idx = spliced.find("NEW").unwrap();
        assert!(new_idx < sect_idx);
    }

    #[test]
    fn title_containing_language_token_is_not_rescanned() {
        // Historical bug: the `.replace("__TITLE__", t).replace("__LANGUAGE__", l)`
        // chain would turn a title of "foo __LANGUAGE__ bar" into "foo en-US bar".
        // The single-pass render_template substitutes each token once.
        let core = crate::tools::viz::escape::render_template(
            CORE_TEMPLATE,
            &[
                ("TITLE", &xml_escape("foo __LANGUAGE__ bar")),
                ("LANGUAGE", &xml_escape("en-US")),
            ],
        );
        assert!(
            core.contains("<dc:title>foo __LANGUAGE__ bar</dc:title>"),
            "{core}"
        );
        assert!(core.contains("<dc:language>en-US</dc:language>"), "{core}");
    }

    #[test]
    fn a_markdown_link_becomes_a_hyperlink_and_a_relationship() {
        let mut links = LinkCollector::starting_at(FIRST_FREE_REL_ID);
        let xml = render_inline_runs("see [the docs](https://example.com/x) now", &mut links);

        assert!(xml.contains(r#"<w:hyperlink r:id="rId3">"#), "{xml}");
        assert!(xml.contains(r#"<w:rStyle w:val="Hyperlink"/>"#), "{xml}");
        assert!(xml.contains("the docs"), "{xml}");
        // Text either side survives untouched.
        assert!(xml.contains("see "), "{xml}");
        assert!(xml.contains(" now"), "{xml}");

        let rels = links.relationships_xml();
        assert!(rels.contains(r#"Id="rId3""#), "{rels}");
        assert!(rels.contains(r#"Target="https://example.com/x""#), "{rels}");
        assert!(rels.contains(r#"TargetMode="External""#), "{rels}");
    }

    /// The document is opened later in Word, so a `javascript:` target is one
    /// this writer simply does not emit. The URL stays readable as text.
    #[test]
    fn an_unsafe_link_target_is_written_as_text_not_a_link() {
        for url in [
            "javascript:alert(1)",
            "vbscript:msgbox",
            "file://server/share/x",
            "data:text/html,<script>",
        ] {
            let mut links = LinkCollector::starting_at(FIRST_FREE_REL_ID);
            let xml = render_inline_runs(&format!("[click]({url})"), &mut links);
            assert!(
                !xml.contains("<w:hyperlink"),
                "{url} produced a link: {xml}"
            );
            assert!(links.is_empty(), "{url} allocated a relationship");
            assert!(xml.contains("click"), "the label must survive: {xml}");
        }
    }

    #[test]
    fn several_links_get_distinct_ids_in_document_order() {
        let mut links = LinkCollector::starting_at(FIRST_FREE_REL_ID);
        let xml = render_body(
            "[one](https://a.example) and [two](https://b.example)\n\n- [three](mailto:x@y.example)",
            &mut links,
        );
        for id in ["rId3", "rId4", "rId5"] {
            assert!(
                xml.contains(&format!(r#"r:id="{id}""#)),
                "missing {id}: {xml}"
            );
        }
        let rels = links.relationships_xml();
        assert!(rels.contains("https://a.example"));
        assert!(rels.contains("https://b.example"));
        assert!(rels.contains("mailto:x@y.example"));
    }

    /// Emphasis inside a label must not be mistaken for the label's end, and a
    /// link must not swallow the rest of the sentence.
    #[test]
    fn links_and_emphasis_do_not_interfere() {
        let mut links = LinkCollector::starting_at(FIRST_FREE_REL_ID);
        let xml = render_inline_runs(
            "**bold** then [a b](https://x.example) then *it*",
            &mut links,
        );
        assert!(xml.contains("<w:b/>"), "{xml}");
        assert!(xml.contains("<w:i/>"), "{xml}");
        assert_eq!(xml.matches("<w:hyperlink").count(), 1, "{xml}");
        assert!(xml.contains(" then "), "{xml}");
    }

    /// A URL with `&` in it has to be escaped in both the run text and the
    /// relationship target, or the package is not well-formed XML.
    #[test]
    fn an_ampersand_in_a_target_is_escaped_in_the_relationship() {
        let mut links = LinkCollector::starting_at(FIRST_FREE_REL_ID);
        let _ = render_inline_runs("[q](https://x.example/s?a=1&b=2)", &mut links);
        let rels = links.relationships_xml();
        assert!(rels.contains("a=1&amp;b=2"), "{rels}");
        assert!(!rels.contains("a=1&b=2"), "raw ampersand in XML: {rels}");
    }

    /// A relationship id must clear every id already in the package, including
    /// ones this writer never emits (images, headers) and non-contiguous ones
    /// Word leaves behind when a relationship is deleted.
    #[test]
    fn new_relationship_ids_clear_every_existing_one() {
        let rels = concat!(
            r#"<Relationships><Relationship Id="rId1" Target="styles.xml"/>"#,
            r#"<Relationship Id="rId9" Target="media/image1.png"/>"#,
            r#"<Relationship Id="rId4" Target="header1.xml"/></Relationships>"#
        );
        assert_eq!(next_free_rel_id(rels), 10);
        // An empty or unrecognisable rels part still starts above the two
        // static relationships `create` writes.
        assert_eq!(next_free_rel_id("<Relationships/>"), FIRST_FREE_REL_ID);
        assert_eq!(next_free_rel_id(DOCUMENT_RELS), FIRST_FREE_REL_ID);
    }

    #[test]
    fn relationships_are_spliced_before_the_closing_tag() {
        let mut links = LinkCollector::starting_at(3);
        links.add("https://x.example");
        let out = insert_relationships(DOCUMENT_RELS, &links);
        assert!(out.contains(r#"Id="rId3""#));
        assert!(out.trim_end().ends_with("</Relationships>"));
        // Existing entries survive.
        assert!(out.contains(r#"Id="rId1""#));
        assert!(out.contains(r#"Id="rId2""#));
    }

    /// With no links, every part must come out byte-identical to before this
    /// feature existed — an append that adds a plain paragraph must not
    /// rewrite the package's relationships, root element or styles.
    #[test]
    fn a_document_with_no_links_is_left_untouched() {
        let empty = LinkCollector::default();
        assert_eq!(insert_relationships(DOCUMENT_RELS, &empty), DOCUMENT_RELS);
    }

    /// A file written by an older build has no `xmlns:r`, and an `r:id`
    /// against an undeclared prefix is not well-formed XML — Word refuses to
    /// open it at all.
    #[test]
    fn appending_a_link_declares_the_namespace_if_it_is_missing() {
        let old = r#"<?xml version="1.0"?><w:document xmlns:w="http://x"><w:body/></w:document>"#;
        let patched = ensure_relationship_namespace(old);
        assert!(patched.contains("xmlns:r="), "{patched}");
        assert!(
            patched.contains("<w:body/>"),
            "the body must survive: {patched}"
        );

        // Already declared: left exactly as it was.
        let current = format!(r#"<w:document {DOCUMENT_XML_NS}><w:body/></w:document>"#);
        assert_eq!(ensure_relationship_namespace(&current), current);
    }

    /// A `w:rStyle` naming a style the package doesn't define is legal but
    /// renders as body text — the link would work and look like nothing.
    #[test]
    fn appending_a_link_adds_the_hyperlink_style_if_it_is_missing() {
        let bare = r#"<w:styles><w:style w:styleId="Normal"/></w:styles>"#;
        let patched = ensure_hyperlink_style(bare);
        assert!(patched.contains(r#"w:styleId="Hyperlink""#), "{patched}");
        assert!(patched.contains(r#"w:styleId="Normal""#), "{patched}");
        // The shipped template already has it; adding a second would be invalid.
        assert_eq!(
            ensure_hyperlink_style(STYLES_TEMPLATE)
                .matches(r#"w:styleId="Hyperlink""#)
                .count(),
            1
        );
    }

    /// The create-mode template must define the style the writer references,
    /// or every link in a new document renders as plain body text.
    #[test]
    fn the_shipped_styles_define_the_hyperlink_style() {
        assert!(STYLES_TEMPLATE.contains(r#"w:styleId="Hyperlink""#));
    }

    /// A link inside a table cell is exactly where a model puts a source, and
    /// cells used to render without any inline handling at all.
    #[test]
    fn a_link_inside_a_table_cell_is_rendered() {
        let mut links = LinkCollector::starting_at(FIRST_FREE_REL_ID);
        let body = render_body(
            "| name | source |\n|---|---|\n| a | [ref](https://x.example) |",
            &mut links,
        );
        assert!(body.contains("<w:hyperlink"), "{body}");
        assert_eq!(
            links
                .relationships_xml()
                .matches("Relationship Id=")
                .count(),
            1
        );
    }

    /// A label-less link would be invisible and unclickable; show the target.
    #[test]
    fn an_empty_label_falls_back_to_showing_the_url() {
        let mut links = LinkCollector::starting_at(FIRST_FREE_REL_ID);
        let xml = render_inline_runs("[](https://x.example/page)", &mut links);
        assert!(xml.contains("<w:hyperlink"), "{xml}");
        assert!(xml.contains("https://x.example/page"), "{xml}");
    }
}
