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

const DOCUMENT_XML_NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006""#;

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

fn render_inline_runs(text: &str) -> String {
    // Non-greedy, bold (**) checked before italic (*) — matches
    // `re.split(r"(\*\*.*?\*\*|\*.*?\*)", text)`. A lone `*` with no partner
    // falls through as a literal token, same as the Python version.
    static PATTERN: &str = r"(\*\*.*?\*\*|\*.*?\*)";
    let re = Regex::new(PATTERN).unwrap();

    let mut out = String::new();
    let mut last = 0;
    for m in re.find_iter(text) {
        if m.start() > last {
            out.push_str(&plain_run(&text[last..m.start()]));
        }
        let token = m.as_str();
        if token.starts_with("**") && token.ends_with("**") && token.len() >= 4 {
            out.push_str(&run(&token[2..token.len() - 2], true, false));
        } else if token.starts_with('*') && token.ends_with('*') && token.len() >= 2 {
            out.push_str(&run(&token[1..token.len() - 1], false, true));
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

fn plain_run(text: &str) -> String {
    run(text, false, false)
}

fn run(text: &str, bold: bool, italic: bool) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut rpr = String::new();
    if bold || italic {
        rpr.push_str("<w:rPr>");
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

fn heading_xml(level: u32, text: &str) -> String {
    // level 0 = "Title" style (python-docx's `add_heading(title, level=0)`
    // convention, reproduced literally — the base plan notes `create` mode
    // always emits this first).
    let style = if level == 0 {
        "Title".to_string()
    } else {
        format!("Heading{}", level.min(4))
    };
    paragraph_xml(Some(&style), "", &render_inline_runs(text))
}

fn list_paragraph_xml(style_id: &str, num_id: u32, text: &str) -> String {
    let num_pr = format!(r#"<w:numPr><w:ilvl w:val="0"/><w:numId w:val="{num_id}"/></w:numPr>"#);
    paragraph_xml(Some(style_id), &num_pr, &render_inline_runs(text))
}

/// A markdown-lite table block: consumes lines only while they both start
/// and end with `|`. The separator row (`^\|[\s\-:\t|]+\|$`) is skipped, not
/// rendered as data. `num_cols = max(len(row))` so short rows leave
/// trailing cells empty. Row 0 gets `w:tblHeader`; **every** row (row 0
/// included) gets `w:cantSplit` — both ported literally from
/// `_make_table_accessible`, which is exactly what it did.
fn table_xml(rows: &[Vec<String>]) -> String {
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
                paragraph_xml(None, "", &plain_run(cell_text))
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

fn is_table_separator(line: &str) -> bool {
    static SEP: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = SEP.get_or_init(|| Regex::new(r"^\|[\s\-:\t|]+\|$").unwrap());
    re.is_match(line)
}

/// Renders `doc_text` into a sequence of body-XML fragments, mirroring
/// `word_write_doc`'s line dispatch loop exactly (table detection,
/// heading levels longest-prefix-first, bullet/number lists, else Normal).
fn render_body(doc_text: &str) -> String {
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
                out.push_str(&table_xml(&rows));
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix("#### ") {
            out.push_str(&heading_xml(4, rest));
        } else if let Some(rest) = line.strip_prefix("### ") {
            out.push_str(&heading_xml(3, rest));
        } else if let Some(rest) = line.strip_prefix("## ") {
            out.push_str(&heading_xml(2, rest));
        } else if let Some(rest) = line.strip_prefix("# ") {
            out.push_str(&heading_xml(1, rest));
        } else if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            out.push_str(&list_paragraph_xml("ListBullet", 1, rest));
        } else if let Some(rest) = strip_ordered_list_prefix(line) {
            out.push_str(&list_paragraph_xml("ListNumber", 2, rest));
        } else {
            out.push_str(&paragraph_xml(
                Some("Normal"),
                "",
                &render_inline_runs(line),
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

    let mut body = heading_xml(0, &doc_title);
    if let Some(text) = doc_text {
        if !text.trim().is_empty() {
            body.push_str(&render_body(text));
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
        DOCUMENT_RELS,
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

    let mut new_body = String::new();
    if let Some(text) = doc_text {
        if !text.trim().is_empty() {
            new_body = render_body(text);
        }
    }

    let spliced = splice_before_sect_pr(&document_xml, &new_body);
    parts.insert("word/document.xml".to_string(), spliced.into_bytes());

    // `_set_doc_accessibility_meta` runs on append too — the language
    // append hits styles.xml regardless of `title`; the title change to
    // core.xml is conditional on `title` being given, matching
    // `if title: doc.core_properties.title = title`.
    if let Some(styles_bytes) = parts.get("word/styles.xml").cloned() {
        let styles_str = String::from_utf8_lossy(&styles_bytes).to_string();
        let patched = append_lang_to_styles_root(&styles_str, language);
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
        let xml = render_inline_runs("**bold** and *italic* text");
        assert!(xml.contains("<w:b/>"));
        assert!(xml.contains("<w:i/>"));
        assert!(xml.contains(">bold<"));
        assert!(xml.contains(">italic<"));
    }

    #[test]
    fn lone_asterisk_is_literal() {
        let xml = render_inline_runs("a * b");
        assert!(!xml.contains("<w:b/>"));
        assert!(!xml.contains("<w:i/>"));
    }

    #[test]
    fn heading_dispatch_prefers_longest_prefix_first() {
        let body = render_body("#### four\n### three\n## two\n# one");
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
        let xml = table_xml(&rows);
        // 2 columns (max row len), second row's second cell empty.
        assert_eq!(xml.matches("<w:gridCol").count(), 2);
    }

    #[test]
    fn table_header_and_cant_split_on_every_row() {
        let rows = vec![vec!["h".to_string()], vec!["v".to_string()]];
        let xml = table_xml(&rows);
        assert_eq!(xml.matches("<w:tblHeader/>").count(), 1);
        assert_eq!(xml.matches("<w:cantSplit/>").count(), 2);
    }

    #[test]
    fn table_separator_row_is_not_rendered_as_data() {
        let body = render_body("| a | b |\n|---|---|\n| 1 | 2 |");
        assert!(body.contains(">a<"));
        assert!(body.contains(">1<"));
        assert!(!body.contains("---"));
    }

    #[test]
    fn bare_pipe_line_does_not_panic() {
        // Audit #114: a lone "|" passed the table gate and panicked slicing
        // `[1..0]`. It now renders as one empty cell, mirroring Python's
        // forgiving `line[1:-1]`.
        let body = render_body("|");
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
}
