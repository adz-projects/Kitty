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
use std::path::{Path, PathBuf};

use regex::Regex;
use zip::write::SimpleFileOptions;
use zip::ZipArchive;

use super::DocxError;

const CONTENT_TYPES: &str = include_str!("assets/content_types.xml");
const ROOT_RELS: &str = include_str!("assets/root_rels.xml");
const DOCUMENT_RELS: &str = include_str!("assets/document_rels.xml");
const NUMBERING: &str = include_str!("assets/numbering.xml");
const STYLES_TEMPLATE: &str = include_str!("assets/styles.xml");
const STYLES_SERIF_TEMPLATE: &str = include_str!("assets/styles_serif.xml");
const FOOTER_XML: &str = include_str!("assets/footer1.xml");
const CORE_TEMPLATE: &str = include_str!("assets/core.xml");
const APP_PROPS: &str = include_str!("assets/app.xml");

const DOCUMENT_XML_NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006""#;

const REL_HYPERLINK: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink";
const REL_IMAGE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image";
const REL_FOOTNOTES: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes";
const REL_FOOTER: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer";

pub enum WriteMode {
    Create,
    Append,
}

pub struct WriteResult {
    pub path: String,
    pub mode: &'static str,
    pub language: String,
}

/// Selectable style templates with considered fonts. `Default` is a grayscale
/// editorial layout (black Montserrat titles/headings, Open Sans body text,
/// white-on-black table headers); `Serif` is a bookish Georgia. Both meet the
/// same accessibility structure (heading outline levels, table headers, lang).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StyleTemplate {
    #[default]
    Default,
    Serif,
}

impl StyleTemplate {
    fn styles_xml(self) -> &'static str {
        match self {
            StyleTemplate::Default => STYLES_TEMPLATE,
            StyleTemplate::Serif => STYLES_SERIF_TEMPLATE,
        }
    }
}

/// Options for [`write_document_with_options`]. `base_dir` resolves relative
/// image paths in the markdown; `None` uses the destination's parent.
/// `image_gate`, when set, must accept a resolved image path before it is
/// read and embedded: a caller that writes on a model's behalf passes its
/// path allow-list here, so `![x](C:/elsewhere/private.png)` cannot copy a
/// file from outside the granted folders into the document.
#[derive(Debug, Clone, Default)]
pub struct WriteOptions {
    pub template: StyleTemplate,
    pub base_dir: Option<PathBuf>,
    pub image_gate: Option<fn(&Path) -> bool>,
}

pub fn write_document(
    path: &Path,
    doc_text: Option<&str>,
    mode: WriteMode,
    title: Option<&str>,
    language: &str,
) -> Result<WriteResult, DocxError> {
    write_document_with_options(
        path,
        doc_text,
        mode,
        title,
        language,
        &WriteOptions::default(),
    )
}

/// Additive entry point: the original [`write_document`] delegates here with
/// default options, so every existing call site is unchanged.
pub fn write_document_with_options(
    path: &Path,
    doc_text: Option<&str>,
    mode: WriteMode,
    title: Option<&str>,
    language: &str,
    options: &WriteOptions,
) -> Result<WriteResult, DocxError> {
    match mode {
        WriteMode::Create => create(path, doc_text, title, language, options),
        WriteMode::Append => append(path, doc_text, title, language, options),
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
// Markdown -> OOXML body renderer
// ---------------------------------------------------------------------------

/// A relationship the package needs, of any kind. Ids start above whatever
/// the package already uses, because reusing an id would silently repoint an
/// existing relationship (styles, numbering, an image) at a new target.
#[derive(Debug, Clone)]
enum Rel {
    Hyperlink { id: String, url: String },
    Image { id: String, target: String },
    Footnotes { id: String },
    Footer { id: String },
}

/// Relationships discovered while rendering a body, in the order they appear.
///
/// A `w:hyperlink` does not carry its URL: it carries an `r:id` pointing at a
/// relationship in `word/_rels/document.xml.rels`, so the target lives in a
/// different part of the package from the text that links to it. Rendering
/// therefore has to *collect* as it goes and hand the caller the relationship
/// entries to write alongside the document. Images and the footnotes part use
/// the same id space.
#[derive(Debug, Default)]
pub(crate) struct LinkCollector {
    next_id: u32,
    rels: Vec<Rel>,
}

impl LinkCollector {
    fn starting_at(next_id: u32) -> Self {
        Self {
            next_id,
            rels: Vec::new(),
        }
    }

    fn alloc_id(&mut self) -> String {
        let id = format!("rId{}", self.next_id);
        self.next_id += 1;
        id
    }

    fn add(&mut self, url: &str) -> String {
        let id = self.alloc_id();
        self.rels.push(Rel::Hyperlink {
            id: id.clone(),
            url: url.to_string(),
        });
        id
    }

    fn add_image(&mut self, target: &str) -> String {
        let id = self.alloc_id();
        self.rels.push(Rel::Image {
            id: id.clone(),
            target: target.to_string(),
        });
        id
    }

    fn add_footnotes(&mut self) -> String {
        let id = self.alloc_id();
        self.rels.push(Rel::Footnotes { id: id.clone() });
        id
    }

    fn add_footer(&mut self) -> String {
        let id = self.alloc_id();
        self.rels.push(Rel::Footer { id: id.clone() });
        id
    }

    fn is_empty(&self) -> bool {
        self.rels.is_empty()
    }

    /// The `<Relationship>` elements to splice into `document.xml.rels`.
    fn relationships_xml(&self) -> String {
        self.rels
            .iter()
            .map(|rel| match rel {
                Rel::Hyperlink { id, url } => format!(
                    r#"<Relationship Id="{id}" Type="{REL_HYPERLINK}" Target="{}" TargetMode="External"/>"#,
                    xml_escape(url)
                ),
                Rel::Image { id, target } => format!(
                    r#"<Relationship Id="{id}" Type="{REL_IMAGE}" Target="{}"/>"#,
                    xml_escape(target)
                ),
                Rel::Footnotes { id } => format!(
                    r#"<Relationship Id="{id}" Type="{REL_FOOTNOTES}" Target="footnotes.xml"/>"#
                ),
                Rel::Footer { id } => format!(
                    r#"<Relationship Id="{id}" Type="{REL_FOOTER}" Target="footer1.xml"/>"#
                ),
            })
            .collect()
    }
}

/// An image to embed in the package.
#[derive(Debug, Clone)]
struct ImageAsset {
    part_name: String,
    bytes: Vec<u8>,
    alt: String,
    width_emu: u64,
    height_emu: u64,
    rel_id: String,
    doc_pr_id: u32,
}

/// Everything the renderer accumulates while walking the markdown.
#[derive(Debug, Default)]
pub(crate) struct RenderCtx {
    links: LinkCollector,
    images: Vec<ImageAsset>,
    /// Footnote definitions collected from `[^label]: text` lines.
    footnote_defs: BTreeMap<String, String>,
    /// Footnote references in order of appearance: (id, label).
    footnote_refs: Vec<(u32, String)>,
    next_footnote_id: u32,
    base_dir: Option<PathBuf>,
    image_gate: Option<fn(&Path) -> bool>,
    /// Numbers already taken in the package, so an append never overwrites an
    /// existing `word/media/imageN` part or repeats a drawing's `docPr` id.
    first_image_index: usize,
    first_doc_pr_id: u32,
}

impl RenderCtx {
    fn with_links(links: LinkCollector, base_dir: Option<PathBuf>) -> Self {
        Self {
            links,
            base_dir,
            next_footnote_id: 1,
            first_image_index: 1,
            first_doc_pr_id: 1,
            ..Default::default()
        }
    }

    fn footnote_id(&mut self, label: &str) -> Option<u32> {
        if !self.footnote_defs.contains_key(label) {
            return None;
        }
        if let Some((id, _)) = self.footnote_refs.iter().find(|(_, l)| l == label) {
            return Some(*id);
        }
        let id = self.next_footnote_id;
        self.next_footnote_id += 1;
        self.footnote_refs.push((id, label.to_string()));
        Some(id)
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

/// Character formatting carried by a run: the emphasis the markdown around it
/// asked for. Emphasis nests (`**~~both~~**`), so it is accumulated while
/// descending into a token rather than decided per token.
#[derive(Clone, Copy, Default)]
struct Fmt {
    bold: bool,
    italic: bool,
    strike: bool,
}

/// Emphasis-only inline rendering of `text` under `fmt`: `**bold**`, `*italic*`
/// and `~~strikethrough~~`, each of which may contain the others. Anything
/// else is literal text carrying `fmt`.
fn emphasis_runs(text: &str, fmt: Fmt) -> String {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(~~.+?~~|\*\*.*?\*\*|\*.*?\*)").unwrap());

    let mut out = String::new();
    let mut last = 0;
    for m in re.find_iter(text) {
        if m.start() > last {
            out.push_str(&run_fmt(&text[last..m.start()], fmt, None));
        }
        out.push_str(&emphasis_token(m.as_str(), fmt));
        last = m.end();
    }
    if last < text.len() {
        out.push_str(&run_fmt(&text[last..], fmt, None));
    }
    out
}

/// One matched emphasis token (`~~x~~`, `**x**` or `*x*`), rendered with the
/// matching format added to `fmt`. Anything that is not one of those is text.
fn emphasis_token(token: &str, fmt: Fmt) -> String {
    if token.starts_with("~~") && token.ends_with("~~") && token.len() >= 5 {
        emphasis_runs(
            &token[2..token.len() - 2],
            Fmt {
                strike: true,
                ..fmt
            },
        )
    } else if token.starts_with("**") && token.ends_with("**") && token.len() >= 4 {
        emphasis_runs(&token[2..token.len() - 2], Fmt { bold: true, ..fmt })
    } else if token.starts_with('*') && token.ends_with('*') && token.len() >= 2 {
        emphasis_runs(
            &token[1..token.len() - 1],
            Fmt {
                italic: true,
                ..fmt
            },
        )
    } else {
        run_fmt(token, fmt, None)
    }
}

fn render_inline_runs(text: &str, ctx: &mut RenderCtx) -> String {
    // Images first (so `![a](b)` is not chewed by the link pattern), then
    // footnote refs, links, inline code, strikethrough, bold, italic. The link
    // target stops at whitespace or `)`, which keeps `see [docs](https://x) for
    // more` from swallowing the rest of the line.
    static PATTERN: &str = r"(!\[[^\]]*\]\([^)\s]*\)|\[\^[^\]]+\]|\[[^\]]*\]\([^)\s]*\)|`[^`]+`|~~.+?~~|\*\*.*?\*\*|\*.*?\*)";
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(PATTERN).unwrap());

    let mut out = String::new();
    let mut last = 0;
    for m in re.find_iter(text) {
        if m.start() > last {
            out.push_str(&plain_run(&text[last..m.start()]));
        }
        let token = m.as_str();
        if let Some(rest) = token.strip_prefix("![") {
            out.push_str(&image_or_plain(rest, ctx));
        } else if let Some(rest) = token.strip_prefix("[^") {
            let label = rest.strip_suffix(']').unwrap_or(rest);
            match ctx.footnote_id(label) {
                Some(id) => out.push_str(&footnote_reference_run(id)),
                None => out.push_str(&plain_run(token)),
            }
        } else if let Some((label, url)) = parse_markdown_link(token) {
            out.push_str(&link_or_plain(label, url, &mut ctx.links));
        } else if token.starts_with('`') && token.ends_with('`') && token.len() >= 2 {
            out.push_str(&run(
                &token[1..token.len() - 1],
                false,
                false,
                Some("CodeChar"),
            ));
        } else if token.starts_with("~~") || token.starts_with('*') {
            out.push_str(&emphasis_token(token, Fmt::default()));
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

/// `alt](src)` (the `![` already stripped) -> an embedded image when the file
/// resolves, else a readable placeholder. The alt text rides on `wp:docPr`
/// and `pic:cNvPr` `descr`, which is what screen readers read.
fn image_or_plain(rest: &str, ctx: &mut RenderCtx) -> String {
    let Some(close) = rest.find("](") else {
        return plain_run(&format!("![{rest}"));
    };
    let alt = &rest[..close];
    let Some(src) = rest[close + 2..].strip_suffix(')') else {
        return plain_run(&format!("![{rest}"));
    };
    let resolved =
        resolve_image_path(src, ctx.base_dir.as_deref()).filter(|p| match ctx.image_gate {
            Some(gate) => gate(p),
            None => true,
        });
    let Some(path) = resolved else {
        return plain_run(&format!("[image: {alt} ({src})]"));
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return plain_run(&format!("[image: {alt} ({src})]"));
    };
    let Some((mime, ext)) = image_mime(&bytes) else {
        return plain_run(&format!("[image: {alt} ({src})]"));
    };
    let (w_px, h_px) = image_dimensions(&bytes).unwrap_or((800, 600));
    // 96 dpi: 1 px = 9525 EMU. Cap the width at 5.5" so a huge image fits the
    // page; scale the height proportionally.
    let mut width_emu = w_px as u64 * 9525;
    let mut height_emu = h_px as u64 * 9525;
    const MAX_WIDTH: u64 = 5_029_200; // 5.5 inches
    if width_emu > MAX_WIDTH {
        height_emu = height_emu * MAX_WIDTH / width_emu.max(1);
        width_emu = MAX_WIDTH;
    }
    let index = ctx.first_image_index + ctx.images.len();
    let part_name = format!("word/media/image{index}.{ext}");
    let rel_id = ctx.links.add_image(&format!("media/image{index}.{ext}"));
    let asset = ImageAsset {
        part_name,
        bytes,
        alt: alt.to_string(),
        width_emu,
        height_emu,
        rel_id,
        doc_pr_id: ctx.first_doc_pr_id + ctx.images.len() as u32,
    };
    let xml = drawing_xml(&asset, mime);
    ctx.images.push(asset);
    xml
}

fn resolve_image_path(src: &str, base_dir: Option<&Path>) -> Option<PathBuf> {
    if src.starts_with("http://") || src.starts_with("https://") || src.starts_with("data:") {
        return None; // remote images are not fetched by the deterministic writer
    }
    let p = Path::new(src);
    if p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    let base = base_dir?;
    let joined = base.join(p);
    joined.is_file().then_some(joined)
}

fn image_mime(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some(("image/png", "png"))
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(("image/jpeg", "jpeg"))
    } else if bytes.starts_with(b"GIF8") {
        Some(("image/gif", "gif"))
    } else {
        None
    }
}

fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let cursor = std::io::Cursor::new(bytes);
    let reader = image::ImageReader::new(cursor).with_guessed_format().ok()?;
    reader.into_dimensions().ok()
}

fn drawing_xml(asset: &ImageAsset, _mime: &str) -> String {
    let alt = xml_escape(&asset.alt);
    let name = format!("Picture {}", asset.doc_pr_id);
    format!(
        concat!(
            r#"<w:r><w:drawing>"#,
            r#"<wp:inline xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" distT="0" distB="0" distL="0" distR="0">"#,
            r#"<wp:extent cx="{cx}" cy="{cy}"/>"#,
            r#"<wp:effectExtent l="0" t="0" r="0" b="0"/>"#,
            r#"<wp:docPr id="{id}" name="{name}" descr="{alt}"/>"#,
            r#"<wp:cNvGraphicFramePr><a:graphicFrameLocks xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" noChangeAspect="1"/></wp:cNvGraphicFramePr>"#,
            r#"<a:graphic xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#,
            r#"<a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture">"#,
            r#"<pic:pic xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture">"#,
            r#"<pic:nvPicPr><pic:cNvPr id="{id}" name="{name}" descr="{alt}"/><pic:cNvPicPr/></pic:nvPicPr>"#,
            r#"<pic:blipFill><a:blip r:embed="{rel}"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>"#,
            r#"<pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm>"#,
            r#"<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr>"#,
            r#"</pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r>"#
        ),
        cx = asset.width_emu,
        cy = asset.height_emu,
        id = asset.doc_pr_id,
        name = xml_escape(&name),
        alt = alt,
        rel = asset.rel_id,
    )
}

fn footnote_reference_run(id: u32) -> String {
    format!(
        r#"<w:r><w:rPr><w:rStyle w:val="FootnoteReference"/></w:rPr><w:footnoteReference w:id="{id}"/></w:r>"#
    )
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
    run_fmt(
        text,
        Fmt {
            bold,
            italic,
            strike: false,
        },
        style,
    )
}

fn run_fmt(text: &str, fmt: Fmt, style: Option<&str>) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut rpr = String::new();
    if fmt.bold || fmt.italic || fmt.strike || style.is_some() {
        rpr.push_str("<w:rPr>");
        // `w:rPr` is an ordered sequence and Word rejects a document that gets
        // it wrong: `rStyle`, then `b`, then `i`, then `strike`.
        if let Some(style) = style {
            rpr.push_str(&format!(r#"<w:rStyle w:val="{style}"/>"#));
        }
        if fmt.bold {
            rpr.push_str("<w:b/>");
        }
        if fmt.italic {
            rpr.push_str("<w:i/>");
        }
        if fmt.strike {
            rpr.push_str("<w:strike/>");
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

fn heading_xml(level: u32, text: &str, ctx: &mut RenderCtx) -> String {
    // level 0 = "Title" style (python-docx's `add_heading(title, level=0)`
    // convention, reproduced literally — the base plan notes `create` mode
    // always emits this first). Headings 1–6 map to their own styles.
    let style = if level == 0 {
        "Title".to_string()
    } else {
        format!("Heading{}", level.min(6))
    };
    paragraph_xml(Some(&style), "", &render_inline_runs(text, ctx))
}

fn list_paragraph_xml(
    style_id: &str,
    num_id: u32,
    ilvl: u32,
    text: &str,
    ctx: &mut RenderCtx,
) -> String {
    let num_pr =
        format!(r#"<w:numPr><w:ilvl w:val="{ilvl}"/><w:numId w:val="{num_id}"/></w:numPr>"#);
    paragraph_xml(Some(style_id), &num_pr, &render_inline_runs(text, ctx))
}

/// A markdown-lite table block: consumes lines only while they both start
/// and end with `|`. The separator row (`^\|[\s\-:\t|]+\|$`) is skipped, not
/// rendered as data. `num_cols = max(len(row))` so short rows leave
/// trailing cells empty. Row 0 gets `w:tblHeader`; **every** row (row 0
/// included) gets `w:cantSplit`.
fn table_xml(rows: &[Vec<String>], ctx: &mut RenderCtx) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let num_cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    if num_cols == 0 {
        return String::new();
    }

    let col_w = (9360 / num_cols).max(720);
    let grid_cols: String = (0..num_cols)
        .map(|_| format!(r#"<w:gridCol w:w="{col_w}"/>"#))
        .collect();

    let mut trs = String::new();
    for (r_idx, row) in rows.iter().enumerate() {
        let is_header = r_idx == 0;
        let mut tr_pr = String::new();
        if is_header {
            tr_pr.push_str("<w:tblHeader/>");
        }
        tr_pr.push_str("<w:cantSplit/>");

        let fill = if is_header {
            "000000"
        } else if r_idx % 2 == 0 {
            "F2F2F2"
        } else {
            "FFFFFF"
        };
        let p_style = if is_header {
            Some("TableHeader")
        } else {
            Some("TableCell")
        };

        let mut cells = String::new();
        for c_idx in 0..num_cols {
            let cell_text = row.get(c_idx).map(String::as_str).unwrap_or("");
            cells.push_str(&format!(
                r#"<w:tc><w:tcPr><w:tcW w:w="{col_w}" w:type="dxa"/><w:shd w:val="clear" w:color="auto" w:fill="{fill}"/><w:vAlign w:val="top"/></w:tcPr>{}</w:tc>"#,
                paragraph_xml(p_style, "", &render_inline_runs(cell_text, ctx))
            ));
        }
        trs.push_str(&format!("<w:tr><w:trPr>{tr_pr}</w:trPr>{cells}</w:tr>"));
    }

    format!(
        r#"<w:tbl><w:tblPr><w:tblStyle w:val="TableGrid"/><w:tblW w:w="9360" w:type="dxa"/><w:tblBorders>
            <w:top w:val="single" w:sz="4" w:space="0" w:color="000000"/>
            <w:left w:val="none" w:sz="0" w:space="0" w:color="auto"/>
            <w:bottom w:val="single" w:sz="10" w:space="0" w:color="000000"/>
            <w:right w:val="none" w:sz="0" w:space="0" w:color="auto"/>
            <w:insideH w:val="single" w:sz="4" w:space="0" w:color="DCDCDC"/>
            <w:insideV w:val="none" w:sz="0" w:space="0" w:color="auto"/>
        </w:tblBorders><w:tblCellMar>
            <w:top w:w="100" w:type="dxa"/>
            <w:left w:w="140" w:type="dxa"/>
            <w:bottom w:w="100" w:type="dxa"/>
            <w:right w:w="140" w:type="dxa"/>
        </w:tblCellMar></w:tblPr><w:tblGrid>{grid_cols}</w:tblGrid>{trs}</w:tbl>"#
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

/// Collect `[^label]: text` footnote definitions into the context. Returns the
/// lines with definitions removed (they render as footnotes, not body text).
fn collect_footnote_defs(lines: &[String], ctx: &mut RenderCtx) -> Vec<String> {
    static DEF: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = DEF.get_or_init(|| Regex::new(r"^\[\^([^\]]+)\]:\s*(.*)$").unwrap());
    let mut kept = Vec::new();
    for line in lines {
        if let Some(caps) = re.captures(line.trim()) {
            let label = caps.get(1).unwrap().as_str().to_string();
            let text = caps.get(2).unwrap().as_str().to_string();
            ctx.footnote_defs.insert(label, text);
        } else {
            kept.push(line.clone());
        }
    }
    kept
}

/// Renders `doc_text` into a sequence of body-XML fragments: tables, headings
/// 1–6, nested bullet/number lists, blockquotes, fenced code, footnote
/// definitions, else Normal paragraphs.
fn render_body(doc_text: &str, ctx: &mut RenderCtx) -> String {
    let raw_lines = crate::text::py_splitlines(doc_text.trim());
    let lines = collect_footnote_defs(&raw_lines, ctx);
    let mut out = String::new();
    let mut i = 0;
    let mut in_fence = false;
    while i < lines.len() {
        let raw = lines[i].as_str();
        let line = raw.trim();

        // Fenced code blocks: emit each inner line verbatim (preserving
        // indentation) as a Code paragraph; the fence markers themselves are
        // not rendered.
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
            i += 1;
            continue;
        }
        if in_fence {
            out.push_str(&paragraph_xml(Some("Code"), "", &plain_run(raw)));
            i += 1;
            continue;
        }

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
                out.push_str(&table_xml(&rows, ctx));
            }
            continue;
        }

        // Indent level for nested lists: two spaces per level, capped at 8.
        let indent = raw.chars().take_while(|c| *c == ' ').count() / 2;
        let ilvl = indent.min(8) as u32;

        if let Some(rest) = line.strip_prefix("###### ") {
            out.push_str(&heading_xml(6, rest, ctx));
        } else if let Some(rest) = line.strip_prefix("##### ") {
            out.push_str(&heading_xml(5, rest, ctx));
        } else if let Some(rest) = line.strip_prefix("#### ") {
            out.push_str(&heading_xml(4, rest, ctx));
        } else if let Some(rest) = line.strip_prefix("### ") {
            out.push_str(&heading_xml(3, rest, ctx));
        } else if let Some(rest) = line.strip_prefix("## ") {
            out.push_str(&heading_xml(2, rest, ctx));
        } else if let Some(rest) = line.strip_prefix("# ") {
            out.push_str(&heading_xml(1, rest, ctx));
        } else if let Some(rest) = line.strip_prefix("> ") {
            out.push_str(&paragraph_xml(
                Some("Quote"),
                "",
                &render_inline_runs(rest, ctx),
            ));
        } else if line == ">" {
            out.push_str(&paragraph_xml(Some("Quote"), "", ""));
        } else if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            out.push_str(&list_paragraph_xml("ListBullet", 1, ilvl, rest, ctx));
        } else if let Some(rest) = strip_ordered_list_prefix(line) {
            out.push_str(&list_paragraph_xml("ListNumber", 2, ilvl, rest, ctx));
        } else {
            out.push_str(&paragraph_xml(
                Some("Normal"),
                "",
                &render_inline_runs(line, ctx),
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

/// The `word/footnotes.xml` part for the collected references. Separator and
/// continuationSeparator footnotes (ids -1 and 0) are required by Word.
fn footnotes_xml(ctx: &RenderCtx) -> String {
    let mut out = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:footnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
    );
    out.push_str(
        r#"<w:footnote w:type="separator" w:id="-1"><w:p><w:r><w:separator/></w:r></w:p></w:footnote>"#,
    );
    out.push_str(
        r#"<w:footnote w:type="continuationSeparator" w:id="0"><w:p><w:r><w:continuationSeparator/></w:r></w:p></w:footnote>"#,
    );
    for (id, label) in &ctx.footnote_refs {
        let text = ctx.footnote_defs.get(label).cloned().unwrap_or_default();
        out.push_str(&format!(
            concat!(
                r#"<w:footnote w:id="{id}"><w:p><w:pPr><w:pStyle w:val="FootnoteText"/></w:pPr>"#,
                r#"<w:r><w:rPr><w:rStyle w:val="FootnoteReference"/></w:rPr><w:footnoteRef/></w:r>"#,
                r#"<w:r><w:t xml:space="preserve"> {text}</w:t></w:r></w:p></w:footnote>"#
            ),
            id = id,
            text = xml_escape(&text),
        ));
    }
    out.push_str("</w:footnotes>");
    out
}

/// Content-type entries for the parts this document actually carries.
fn content_types_with(
    base: &str,
    has_footnotes: bool,
    has_footer: bool,
    image_exts: &[&str],
) -> String {
    let mut additions = String::new();
    if has_footer && !base.contains(r#"PartName="/word/footer1.xml""#) {
        additions.push_str(
            r#"<Override PartName="/word/footer1.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml"/>"#,
        );
    }
    if has_footnotes && !base.contains(r#"PartName="/word/footnotes.xml""#) {
        additions.push_str(
            r#"<Override PartName="/word/footnotes.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.footnotes+xml"/>"#,
        );
    }
    for ext in image_exts {
        let mime = match *ext {
            "png" => "image/png",
            "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            _ => continue,
        };
        // A repeated `Default Extension` makes the package invalid; an append
        // to a document that already holds a PNG must not add another.
        let declared = format!(r#"extension="{ext}""#);
        if base.to_ascii_lowercase().contains(&declared) {
            continue;
        }
        additions.push_str(&format!(
            r#"<Default Extension="{ext}" ContentType="{mime}"/>"#
        ));
    }
    if additions.is_empty() {
        return base.to_string();
    }
    match base.rfind("</Types>") {
        Some(idx) => format!("{}{additions}{}", &base[..idx], &base[idx..]),
        None => base.to_string(),
    }
}

fn image_extensions(images: &[ImageAsset]) -> Vec<&str> {
    let mut exts: Vec<&str> = images
        .iter()
        .filter_map(|i| i.part_name.rsplit('.').next())
        .collect();
    exts.sort_unstable();
    exts.dedup();
    exts
}

// ---------------------------------------------------------------------------
// Create mode
// ---------------------------------------------------------------------------

fn create(
    path: &Path,
    doc_text: Option<&str>,
    title: Option<&str>,
    language: &str,
    options: &WriteOptions,
) -> Result<WriteResult, DocxError> {
    let doc_title = title.map(str::to_string).unwrap_or_else(|| {
        path.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    });
    let base_dir = options
        .base_dir
        .clone()
        .or_else(|| path.parent().map(Path::to_path_buf));

    // Seeded past the two static relationships in `document_rels.xml`
    // (styles = rId1, numbering = rId2) so a link can never collide with one.
    let mut ctx = RenderCtx::with_links(LinkCollector::starting_at(FIRST_FREE_REL_ID), base_dir);
    ctx.image_gate = options.image_gate;
    let mut body = heading_xml(0, &doc_title, &mut ctx);
    if let Some(text) = doc_text {
        if !text.trim().is_empty() {
            body.push_str(&render_body(text, &mut ctx));
        }
    }

    let has_footnotes = !ctx.footnote_refs.is_empty();
    if has_footnotes {
        ctx.links.add_footnotes();
    }
    let footer_rel_id = ctx.links.add_footer();

    let document_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document {DOCUMENT_XML_NS}><w:body>{body}<w:sectPr><w:footerReference w:type="default" r:id="{footer_rel_id}"/><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/></w:sectPr></w:body></w:document>"#
    );

    // WCAG accessibility metadata: `w:lang` in its schema-valid home,
    // `w:docDefaults/w:rPrDefault/w:rPr` (the old writer wrote it as a direct
    // child of the styles root, which Word tolerated but validators rejected).
    let styles_xml = set_lang_in_doc_defaults(options.template.styles_xml(), language);
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
    // Fixed timestamp: Word export must be deterministic (same note, same
    // bytes). The default stamps the current time into every entry.
    let opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default());

    let exts = image_extensions(&ctx.images);
    write_zip_str(
        &mut zip,
        "[Content_Types].xml",
        &content_types_with(CONTENT_TYPES, has_footnotes, true, &exts),
        opts,
    )?;
    write_zip_str(&mut zip, "_rels/.rels", ROOT_RELS, opts)?;
    write_zip_str(
        &mut zip,
        "word/_rels/document.xml.rels",
        &insert_relationships(DOCUMENT_RELS, &ctx.links),
        opts,
    )?;
    write_zip_str(&mut zip, "word/document.xml", &document_xml, opts)?;
    write_zip_str(&mut zip, "word/styles.xml", &styles_xml, opts)?;
    write_zip_str(&mut zip, "word/numbering.xml", NUMBERING, opts)?;
    write_zip_str(&mut zip, "word/footer1.xml", FOOTER_XML, opts)?;
    if has_footnotes {
        write_zip_str(&mut zip, "word/footnotes.xml", &footnotes_xml(&ctx), opts)?;
    }
    for image in &ctx.images {
        zip.start_file(&image.part_name, opts)
            .map_err(|e| DocxError::Corrupt(e.to_string()))?;
        zip.write_all(&image.bytes)
            .map_err(|e| DocxError::Corrupt(e.to_string()))?;
    }
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

/// Sets `<w:lang w:val="...">` in its schema-valid home,
/// `w:docDefaults/w:rPrDefault/w:rPr`. Idempotent: an existing `w:lang` there
/// has its value replaced; a document already carrying the right value comes
/// out byte-identical. This replaces the old bug-for-bug reproduction that
/// wrote `w:lang` as a direct child of the `<w:styles>` root.
fn set_lang_in_doc_defaults(styles_xml: &str, language: &str) -> String {
    let lang = format!(r#"<w:lang w:val="{}"/>"#, xml_escape(language));

    // Already present inside docDefaults: replace the value in place.
    if let Some(dd_start) = styles_xml.find("<w:docDefaults>") {
        if let Some(dd_end) = styles_xml[dd_start..].find("</w:docDefaults>") {
            let dd = &styles_xml[dd_start..dd_start + dd_end];
            static LANG_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
            let re = LANG_RE.get_or_init(|| Regex::new(r#"<w:lang w:val="[^"]*"/>"#).unwrap());
            if re.is_match(dd) {
                let new_dd = re.replace(dd, regex::NoExpand(lang.as_str())).to_string();
                return format!(
                    "{}{}{}",
                    &styles_xml[..dd_start],
                    new_dd,
                    &styles_xml[dd_start + dd_end..]
                );
            }
            // No lang yet: insert before the closing </w:rPr> of rPrDefault.
            if let Some(rpr_default) = dd.find("<w:rPrDefault>") {
                let abs = dd_start + rpr_default;
                if let Some(close) = styles_xml[abs..].find("</w:rPr>") {
                    let at = abs + close;
                    return format!("{}{}{}", &styles_xml[..at], lang, &styles_xml[at..]);
                }
            }
        }
    }

    // No usable docDefaults: prepend a complete one right after the root open.
    if let Some(open_end) = styles_xml.find('>') {
        let block = format!(
            "<w:docDefaults><w:rPrDefault><w:rPr>{lang}</w:rPr></w:rPrDefault></w:docDefaults>"
        );
        return format!(
            "{}{}{}",
            &styles_xml[..open_end + 1],
            block,
            &styles_xml[open_end + 1..]
        );
    }
    styles_xml.to_string()
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
    options: &WriteOptions,
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
    let base_dir = options
        .base_dir
        .clone()
        .or_else(|| path.parent().map(Path::to_path_buf));
    let mut ctx = RenderCtx::with_links(
        LinkCollector::starting_at(next_free_rel_id(&rels_xml)),
        base_dir,
    );
    ctx.image_gate = options.image_gate;
    ctx.first_image_index = next_free_media_index(&parts);
    ctx.first_doc_pr_id = next_free_doc_pr_id(&document_xml);
    let existing_footnotes = parts
        .get("word/footnotes.xml")
        .map(|b| String::from_utf8_lossy(b).to_string());
    if let Some(existing) = &existing_footnotes {
        ctx.next_footnote_id = next_free_footnote_id(existing);
    }

    let mut new_body = String::new();
    if let Some(text) = doc_text {
        if !text.trim().is_empty() {
            new_body = render_body(text, &mut ctx);
        }
    }

    let has_footnotes = !ctx.footnote_refs.is_empty();
    // A package that already has a footnotes part already relates to it; a
    // second footnotes relationship would make the document invalid.
    if has_footnotes && existing_footnotes.is_none() {
        ctx.links.add_footnotes();
    }

    let spliced = splice_before_sect_pr(&document_xml, &new_body);
    // A document written by an older build of this writer has no `xmlns:r` on
    // its root, and `r:id` against an undeclared prefix is not well-formed XML
    // — Word refuses to open the file at all. Only touched when links were
    // actually emitted, so an append with no links leaves the root byte-identical.
    let spliced = if ctx.links.is_empty() {
        spliced
    } else {
        ensure_relationship_namespace(&spliced)
    };
    parts.insert("word/document.xml".to_string(), spliced.into_bytes());

    if !ctx.links.is_empty() {
        parts.insert(
            "word/_rels/document.xml.rels".to_string(),
            insert_relationships(&rels_xml, &ctx.links).into_bytes(),
        );
    }

    // Footnotes: merge into an existing part (continuing its ids) or create one.
    if has_footnotes {
        let merged = match &existing_footnotes {
            Some(existing) => merge_footnotes(existing, &ctx),
            None => footnotes_xml(&ctx),
        };
        parts.insert("word/footnotes.xml".to_string(), merged.into_bytes());
    }

    // Images: add the new media parts.
    for image in &ctx.images {
        parts.insert(image.part_name.clone(), image.bytes.clone());
    }

    // Content types: ensure the new parts are declared.
    if has_footnotes || !ctx.images.is_empty() {
        if let Some(ct_bytes) = parts.get("[Content_Types].xml").cloned() {
            let ct = String::from_utf8_lossy(&ct_bytes).to_string();
            let exts = image_extensions(&ctx.images);
            let patched = content_types_with(&ct, has_footnotes, false, &exts);
            parts.insert("[Content_Types].xml".to_string(), patched.into_bytes());
        }
    }

    // The language metadata step runs on append too, now into the schema-valid
    // docDefaults home; the title change to core.xml is conditional on `title`
    // being given.
    if let Some(styles_bytes) = parts.get("word/styles.xml").cloned() {
        let styles_str = String::from_utf8_lossy(&styles_bytes).to_string();
        let patched = set_lang_in_doc_defaults(&styles_str, language);
        // A `w:rStyle` naming a style the package doesn't define is legal but
        // renders as body text — the link would work and look like nothing.
        let patched = if ctx.links.is_empty() {
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
    // Fixed timestamp: Word export must be deterministic (same note, same
    // bytes). The default stamps the current time into every entry.
    let opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default());
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

/// Merge new footnotes into an existing footnotes.xml, continuing its highest
/// id so references and definitions stay consistent.
/// First `word/media/imageN` number not already used by the package.
fn next_free_media_index(parts: &BTreeMap<String, Vec<u8>>) -> usize {
    parts
        .keys()
        .filter_map(|name| {
            let stem = name.strip_prefix("word/media/image")?;
            stem.split('.').next()?.parse::<usize>().ok()
        })
        .max()
        .map_or(1, |n| n + 1)
}

/// First drawing-object id (`wp:docPr id`) not used in the document. Word
/// reports a file with repeated ids as damaged.
fn next_free_doc_pr_id(document_xml: &str) -> u32 {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"docPr\b[^>]*?\bid="(\d+)""#).unwrap());
    re.captures_iter(document_xml)
        .filter_map(|c| c.get(1)?.as_str().parse::<u32>().ok())
        .max()
        .map_or(1, |n| n + 1)
}

/// First footnote id above every id in an existing footnotes part.
fn next_free_footnote_id(footnotes_xml: &str) -> u32 {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"w:id="(-?\d+)""#).unwrap());
    re.captures_iter(footnotes_xml)
        .filter_map(|c| c.get(1)?.as_str().parse::<i64>().ok())
        .max()
        .map_or(1, |n| (n.max(0) + 1) as u32)
}

fn merge_footnotes(existing: &str, ctx: &RenderCtx) -> String {
    // The ids were allocated above the existing ones while rendering (see
    // `next_free_footnote_id`), so the body's references already point here.
    let mut additions = String::new();
    for (id, label) in &ctx.footnote_refs {
        let text = ctx.footnote_defs.get(label).cloned().unwrap_or_default();
        additions.push_str(&format!(
            concat!(
                r#"<w:footnote w:id="{id}"><w:p><w:pPr><w:pStyle w:val="FootnoteText"/></w:pPr>"#,
                r#"<w:r><w:rPr><w:rStyle w:val="FootnoteReference"/></w:rPr><w:footnoteRef/></w:r>"#,
                r#"<w:r><w:t xml:space="preserve"> {text}</w:t></w:r></w:p></w:footnote>"#
            ),
            id = id,
            text = xml_escape(&text),
        ));
    }
    match existing.rfind("</w:footnotes>") {
        Some(idx) => format!("{}{additions}{}", &existing[..idx], &existing[idx..]),
        None => existing.to_string(),
    }
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

    fn ctx() -> RenderCtx {
        RenderCtx::with_links(LinkCollector::default(), None)
    }

    #[test]
    fn render_inline_runs_handles_bold_then_italic_non_greedy() {
        let xml = render_inline_runs("**bold** and *italic* text", &mut ctx());
        assert!(xml.contains("<w:b/>"));
        assert!(xml.contains("<w:i/>"));
        assert!(xml.contains(">bold<"));
        assert!(xml.contains(">italic<"));
    }

    #[test]
    fn lone_asterisk_is_literal() {
        let xml = render_inline_runs("a * b", &mut ctx());
        assert!(!xml.contains("<w:b/>"));
        assert!(!xml.contains("<w:i/>"));
    }

    #[test]
    fn strikethrough_becomes_a_strike_run() {
        let xml = render_inline_runs("keep ~~drop~~ keep", &mut ctx());
        assert!(
            xml.contains(
                r#"<w:r><w:rPr><w:strike/></w:rPr><w:t xml:space="preserve">drop</w:t></w:r>"#
            ),
            "{xml}"
        );
        // The markers are consumed, and the text around stays plain.
        assert!(!xml.contains("~~"), "{xml}");
        assert!(
            xml.contains(r#"<w:r><w:t xml:space="preserve">keep </w:t></w:r>"#),
            "{xml}"
        );
        assert_eq!(xml.matches("<w:strike/>").count(), 1);
    }

    #[test]
    fn strikethrough_nests_with_bold_and_italic_in_schema_order() {
        // Either nesting order; `w:rPr` must list b, i, strike in that order.
        for src in ["~~**both**~~", "**~~both~~**"] {
            let xml = render_inline_runs(src, &mut ctx());
            assert!(
                xml.contains(
                    r#"<w:rPr><w:b/><w:strike/></w:rPr><w:t xml:space="preserve">both</w:t>"#
                ),
                "{src}: {xml}"
            );
        }
        let all = render_inline_runs("~~a **b *c* d** e~~", &mut ctx());
        assert!(
            all.contains(
                r#"<w:rPr><w:b/><w:i/><w:strike/></w:rPr><w:t xml:space="preserve">c</w:t>"#
            ),
            "{all}"
        );
        assert!(
            all.contains(r#"<w:rPr><w:strike/></w:rPr><w:t xml:space="preserve">a </w:t>"#),
            "{all}"
        );
        assert!(
            all.contains(r#"<w:rPr><w:b/><w:strike/></w:rPr><w:t xml:space="preserve">b </w:t>"#),
            "{all}"
        );
        assert!(
            all.contains(r#"<w:rPr><w:strike/></w:rPr><w:t xml:space="preserve"> e</w:t>"#),
            "{all}"
        );
    }

    #[test]
    fn stray_tildes_stay_literal() {
        for src in ["about ~5 or ~6", "a ~~ b", "~~~~", "x~y"] {
            let xml = render_inline_runs(src, &mut ctx());
            assert!(!xml.contains("<w:strike/>"), "{src}: {xml}");
            assert!(xml.contains('~'), "{src}: {xml}");
        }
    }

    #[test]
    fn strikethrough_renders_in_headings_lists_and_table_cells() {
        let body = render_body(
            "# ~~old~~ new\n\n- ~~gone~~\n\n| a |\n| - |\n| ~~x~~ |",
            &mut ctx(),
        );
        assert_eq!(body.matches("<w:strike/>").count(), 3, "{body}");
        assert!(!body.contains("~~"), "{body}");
    }

    #[test]
    fn emphasis_inside_emphasis_is_now_rendered_not_literal() {
        // Previously the inner markers were written out as text.
        let xml = render_inline_runs("**bold and *italic* inside**", &mut ctx());
        assert!(!xml.contains('*'), "{xml}");
        assert!(
            xml.contains(r#"<w:rPr><w:b/><w:i/></w:rPr><w:t xml:space="preserve">italic</w:t>"#),
            "{xml}"
        );
    }

    #[test]
    fn heading_dispatch_prefers_longest_prefix_first() {
        let body = render_body("#### four\n### three\n## two\n# one", &mut ctx());
        assert!(body.contains(r#"w:val="Heading4""#));
        assert!(body.contains(r#"w:val="Heading3""#));
        assert!(body.contains(r#"w:val="Heading2""#));
        assert!(body.contains(r#"w:val="Heading1""#));
    }

    #[test]
    fn headings_five_and_six_have_their_own_styles() {
        let body = render_body("##### five\n###### six", &mut ctx());
        assert!(body.contains(r#"w:val="Heading5""#));
        assert!(body.contains(r#"w:val="Heading6""#));
    }

    #[test]
    fn blockquote_and_code_fence_render() {
        let body = render_body("> quoted\n\n```rust\nfn a() {}\n```", &mut ctx());
        assert!(body.contains(r#"w:val="Quote""#));
        assert!(body.contains(r#"w:val="Code""#));
        assert!(body.contains("fn a() {}"));
        assert!(!body.contains("```"));
    }

    #[test]
    fn inline_code_uses_the_code_char_style() {
        let xml = render_inline_runs("use `cargo build` now", &mut ctx());
        assert!(xml.contains(r#"w:val="CodeChar""#));
        assert!(xml.contains("cargo build"));
    }

    #[test]
    fn nested_lists_carry_their_indent_level() {
        let body = render_body("- top\n  - nested\n    - deeper", &mut ctx());
        assert!(body.contains(r#"<w:ilvl w:val="0"/>"#));
        assert!(body.contains(r#"<w:ilvl w:val="1"/>"#));
        assert!(body.contains(r#"<w:ilvl w:val="2"/>"#));
    }

    #[test]
    fn footnotes_render_a_reference_and_a_definition() {
        let mut c = ctx();
        let body = render_body("A claim[^1].\n\n[^1]: The source.", &mut c);
        assert!(body.contains(r#"<w:footnoteReference w:id="1"/>"#));
        assert!(!body.contains("The source.")); // definition is not body text
        let fnxml = footnotes_xml(&c);
        assert!(fnxml.contains(r#"<w:footnote w:id="1">"#));
        assert!(fnxml.contains("The source."));
        assert!(fnxml.contains(r#"w:type="separator""#));
    }

    #[test]
    fn an_undefined_footnote_ref_stays_plain_text() {
        let body = render_body("A claim[^missing].", &mut ctx());
        assert!(!body.contains("footnoteReference"));
        assert!(body.contains("[^missing]"));
    }

    #[test]
    fn table_rows_short_row_leaves_trailing_cells_empty() {
        let rows = vec![
            vec!["a".to_string(), "b".to_string()],
            vec!["c".to_string()],
        ];
        let xml = table_xml(&rows, &mut ctx());
        assert_eq!(xml.matches("<w:gridCol").count(), 2);
    }

    #[test]
    fn table_header_and_cant_split_on_every_row() {
        let rows = vec![vec!["h".to_string()], vec!["v".to_string()]];
        let xml = table_xml(&rows, &mut ctx());
        assert_eq!(xml.matches("<w:tblHeader/>").count(), 1);
        assert_eq!(xml.matches("<w:cantSplit/>").count(), 2);
    }

    #[test]
    fn table_separator_row_is_not_rendered_as_data() {
        let body = render_body("| a | b |\n|---|---|\n| 1 | 2 |", &mut ctx());
        assert!(body.contains(">a<"));
        assert!(body.contains(">1<"));
        assert!(!body.contains("---"));
    }

    #[test]
    fn bare_pipe_line_does_not_panic() {
        let body = render_body("|", &mut ctx());
        assert!(
            body.contains("<w:tbl>"),
            "a pipe-gated line still renders as a table: {body}"
        );
    }

    #[test]
    fn title_with_dollar_signs_is_not_mangled_by_regex_replacement() {
        let core = "<cp:coreProperties><dc:title>old</dc:title></cp:coreProperties>";
        let out = set_or_insert_title(core, "Price is $5 and $1 and $$ and ${name}");
        assert!(
            out.contains("<dc:title>Price is $5 and $1 and $$ and ${name}</dc:title>"),
            "title must survive verbatim: {out}"
        );
    }

    #[test]
    fn lang_lands_in_doc_defaults_rpr_not_the_styles_root() {
        let styles = r#"<w:styles xmlns:w="ns"><w:docDefaults><w:rPrDefault><w:rPr><w:sz w:val="22"/></w:rPr></w:rPrDefault></w:docDefaults><w:style/></w:styles>"#;
        let patched = set_lang_in_doc_defaults(styles, "en-US");
        assert!(patched.contains(r#"<w:rPr><w:sz w:val="22"/><w:lang w:val="en-US"/></w:rPr>"#));
        // Not a direct child of the styles root anymore.
        assert!(!patched.ends_with(r#"<w:lang w:val="en-US"/></w:styles>"#));
    }

    #[test]
    fn lang_is_idempotent_and_updates_in_place() {
        let styles = r#"<w:styles><w:docDefaults><w:rPrDefault><w:rPr><w:lang w:val="en-US"/></w:rPr></w:rPrDefault></w:docDefaults></w:styles>"#;
        assert_eq!(set_lang_in_doc_defaults(styles, "en-US"), styles);
        let updated = set_lang_in_doc_defaults(styles, "fr-FR");
        assert!(updated.contains(r#"<w:lang w:val="fr-FR"/>"#));
        assert!(!updated.contains("en-US"));
    }

    #[test]
    fn lang_inserts_doc_defaults_when_absent() {
        let styles = r#"<w:styles xmlns:w="ns"><w:style/></w:styles>"#;
        let patched = set_lang_in_doc_defaults(styles, "de-DE");
        assert!(patched.contains("<w:docDefaults>"));
        assert!(patched.contains(r#"<w:lang w:val="de-DE"/>"#));
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
        let mut c = RenderCtx::with_links(LinkCollector::starting_at(FIRST_FREE_REL_ID), None);
        let xml = render_inline_runs("see [the docs](https://example.com/x) now", &mut c);

        assert!(xml.contains(r#"<w:hyperlink r:id="rId3">"#), "{xml}");
        assert!(xml.contains(r#"<w:rStyle w:val="Hyperlink"/>"#), "{xml}");
        assert!(xml.contains("the docs"), "{xml}");
        assert!(xml.contains("see "), "{xml}");
        assert!(xml.contains(" now"), "{xml}");

        let rels = c.links.relationships_xml();
        assert!(rels.contains(r#"Id="rId3""#), "{rels}");
        assert!(rels.contains(r#"Target="https://example.com/x""#), "{rels}");
        assert!(rels.contains(r#"TargetMode="External""#), "{rels}");
    }

    #[test]
    fn an_unsafe_link_target_is_written_as_text_not_a_link() {
        for url in [
            "javascript:alert(1)",
            "vbscript:msgbox",
            "file://server/share/x",
            "data:text/html,<script>",
        ] {
            let mut c = RenderCtx::with_links(LinkCollector::starting_at(FIRST_FREE_REL_ID), None);
            let xml = render_inline_runs(&format!("[click]({url})"), &mut c);
            assert!(
                !xml.contains("<w:hyperlink"),
                "{url} produced a link: {xml}"
            );
            assert!(c.links.is_empty(), "{url} allocated a relationship");
            assert!(xml.contains("click"), "the label must survive: {xml}");
        }
    }

    #[test]
    fn several_links_get_distinct_ids_in_document_order() {
        let mut c = RenderCtx::with_links(LinkCollector::starting_at(FIRST_FREE_REL_ID), None);
        let xml = render_body(
            "[one](https://a.example) and [two](https://b.example)\n\n- [three](mailto:x@y.example)",
            &mut c,
        );
        for id in ["rId3", "rId4", "rId5"] {
            assert!(
                xml.contains(&format!(r#"r:id="{id}""#)),
                "missing {id}: {xml}"
            );
        }
        let rels = c.links.relationships_xml();
        assert!(rels.contains("https://a.example"));
        assert!(rels.contains("https://b.example"));
        assert!(rels.contains("mailto:x@y.example"));
    }

    #[test]
    fn links_and_emphasis_do_not_interfere() {
        let mut c = RenderCtx::with_links(LinkCollector::starting_at(FIRST_FREE_REL_ID), None);
        let xml = render_inline_runs("**bold** then [a b](https://x.example) then *it*", &mut c);
        assert!(xml.contains("<w:b/>"), "{xml}");
        assert!(xml.contains("<w:i/>"), "{xml}");
        assert_eq!(xml.matches("<w:hyperlink").count(), 1, "{xml}");
        assert!(xml.contains(" then "), "{xml}");
    }

    #[test]
    fn an_ampersand_in_a_target_is_escaped_in_the_relationship() {
        let mut c = RenderCtx::with_links(LinkCollector::starting_at(FIRST_FREE_REL_ID), None);
        let _ = render_inline_runs("[q](https://x.example/s?a=1&b=2)", &mut c);
        let rels = c.links.relationships_xml();
        assert!(rels.contains("a=1&amp;b=2"), "{rels}");
        assert!(!rels.contains("a=1&b=2"), "raw ampersand in XML: {rels}");
    }

    #[test]
    fn new_relationship_ids_clear_every_existing_one() {
        let rels = concat!(
            r#"<Relationships><Relationship Id="rId1" Target="styles.xml"/>"#,
            r#"<Relationship Id="rId9" Target="media/image1.png"/>"#,
            r#"<Relationship Id="rId4" Target="header1.xml"/></Relationships>"#
        );
        assert_eq!(next_free_rel_id(rels), 10);
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
        assert!(out.contains(r#"Id="rId1""#));
        assert!(out.contains(r#"Id="rId2""#));
    }

    #[test]
    fn a_document_with_no_links_is_left_untouched() {
        let empty = LinkCollector::default();
        assert_eq!(insert_relationships(DOCUMENT_RELS, &empty), DOCUMENT_RELS);
    }

    #[test]
    fn appending_a_link_declares_the_namespace_if_it_is_missing() {
        let old = r#"<?xml version="1.0"?><w:document xmlns:w="http://x"><w:body/></w:document>"#;
        let patched = ensure_relationship_namespace(old);
        assert!(patched.contains("xmlns:r="), "{patched}");
        assert!(
            patched.contains("<w:body/>"),
            "the body must survive: {patched}"
        );

        let current = format!(r#"<w:document {DOCUMENT_XML_NS}><w:body/></w:document>"#);
        assert_eq!(ensure_relationship_namespace(&current), current);
    }

    #[test]
    fn appending_a_link_adds_the_hyperlink_style_if_it_is_missing() {
        let bare = r#"<w:styles><w:style w:styleId="Normal"/></w:styles>"#;
        let patched = ensure_hyperlink_style(bare);
        assert!(patched.contains(r#"w:styleId="Hyperlink""#), "{patched}");
        assert!(patched.contains(r#"w:styleId="Normal""#), "{patched}");
        assert_eq!(
            ensure_hyperlink_style(STYLES_TEMPLATE)
                .matches(r#"w:styleId="Hyperlink""#)
                .count(),
            1
        );
    }

    #[test]
    fn the_shipped_styles_define_the_hyperlink_style() {
        assert!(STYLES_TEMPLATE.contains(r#"w:styleId="Hyperlink""#));
    }

    #[test]
    fn the_shipped_styles_define_the_new_phase8_styles() {
        for style in [
            "Heading5",
            "Heading6",
            "Quote",
            "Code",
            "CodeChar",
            "FootnoteText",
            "TableGrid",
        ] {
            assert!(
                STYLES_TEMPLATE.contains(&format!(r#"w:styleId="{style}""#)),
                "missing style {style}"
            );
            assert!(
                STYLES_SERIF_TEMPLATE.contains(&format!(r#"w:styleId="{style}""#)),
                "serif missing style {style}"
            );
        }
    }

    #[test]
    fn a_link_inside_a_table_cell_is_rendered() {
        let mut c = RenderCtx::with_links(LinkCollector::starting_at(FIRST_FREE_REL_ID), None);
        let body = render_body(
            "| name | source |\n|---|---|\n| a | [ref](https://x.example) |",
            &mut c,
        );
        assert!(body.contains("<w:hyperlink"), "{body}");
        assert_eq!(
            c.links
                .relationships_xml()
                .matches("Relationship Id=")
                .count(),
            1
        );
    }

    #[test]
    fn an_empty_label_falls_back_to_showing_the_url() {
        let mut c = RenderCtx::with_links(LinkCollector::starting_at(FIRST_FREE_REL_ID), None);
        let xml = render_inline_runs("[](https://x.example/page)", &mut c);
        assert!(xml.contains("<w:hyperlink"), "{xml}");
        assert!(xml.contains("https://x.example/page"), "{xml}");
    }

    #[test]
    fn a_missing_image_renders_a_readable_placeholder() {
        let mut c = ctx();
        let xml = render_inline_runs("![A chart](no-such-file.png)", &mut c);
        assert!(xml.contains("[image: A chart (no-such-file.png)]"));
        assert!(c.images.is_empty());
    }

    #[test]
    fn an_embedded_image_carries_alt_text_and_a_relationship() {
        let dir = tempfile::tempdir().unwrap();
        // A minimal valid 1x1 PNG.
        let png: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x62, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        std::fs::write(dir.path().join("pic.png"), &png).unwrap();
        let mut c = RenderCtx::with_links(
            LinkCollector::starting_at(FIRST_FREE_REL_ID),
            Some(dir.path().to_path_buf()),
        );
        let xml = render_inline_runs("![A pixel](pic.png)", &mut c);
        assert!(xml.contains("<w:drawing>"), "{xml}");
        assert!(xml.contains(r#"descr="A pixel""#), "{xml}");
        assert_eq!(c.images.len(), 1);
        assert!(c.links.relationships_xml().contains("media/image1.png"));
    }

    #[test]
    fn content_types_gain_footnotes_and_image_entries() {
        let ct = content_types_with(CONTENT_TYPES, true, true, &["png"]);
        assert!(ct.contains("/word/footer1.xml"));
        assert!(ct.contains("/word/footnotes.xml"));
        assert!(ct.contains(r#"Extension="png""#));
        // No additions leaves the base byte-identical.
        assert_eq!(
            content_types_with(CONTENT_TYPES, false, false, &[]),
            CONTENT_TYPES
        );
    }

    #[test]
    fn default_template_uses_grayscale_montserrat_open_sans_and_page_footer() {
        let dir = tempfile::tempdir().unwrap();
        let doc = dir.path().join("styled.docx");
        write_document(
            &doc,
            Some("# One\n\nBody\n\n- a\n- b\n\n| H1 | H2 |\n|---|---|\n| r1 | v1 |\n| r2 | v2 |"),
            WriteMode::Create,
            Some("My Title"),
            "en-US",
        )
        .unwrap();

        let styles = zip_part(&doc, "word/styles.xml").unwrap();
        assert!(styles.contains(r#"w:ascii="Open Sans""#));
        assert!(styles.contains(r#"w:ascii="Montserrat""#));
        assert!(styles.contains(r#"<w:next w:val="Normal"/>"#));
        assert!(styles.contains("<w:contextualSpacing/>"));
        assert!(styles.contains(r#"w:color w:val="0563C1""#));
        // Title 24pt (48), H1 20pt (40), H2 16pt (32), H3 14pt (28).
        assert!(styles.contains(r#"w:sz w:val="48""#));
        assert!(styles.contains(r#"w:sz w:val="40""#));
        assert!(styles.contains(r#"w:sz w:val="32""#));
        assert!(styles.contains(r#"w:sz w:val="28""#));

        let body = zip_part(&doc, "word/document.xml").unwrap();
        assert!(body.contains(r#"w:fill="000000""#));
        assert!(body.contains(r#"w:fill="FFFFFF""#));
        assert!(body.contains(r#"w:fill="F2F2F2""#));
        assert!(body.contains(r#"<w:footerReference w:type="default""#));

        let footer = zip_part(&doc, "word/footer1.xml").unwrap();
        assert!(footer.contains("PAGE"));
        assert!(footer.contains(r#"w:jc w:val="center""#));
        assert!(!footer.contains("NUMPAGES"));
        assert!(!footer.contains("pBdr"));
    }

    fn one_pixel_png() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x62, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
    }

    fn zip_part(path: &Path, name: &str) -> Option<String> {
        let file = std::fs::File::open(path).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let mut entry = archive.by_name(name).ok()?;
        let mut out = String::new();
        entry.read_to_string(&mut out).ok()?;
        Some(out)
    }

    fn zip_names(path: &Path) -> Vec<String> {
        let file = std::fs::File::open(path).unwrap();
        let archive = ZipArchive::new(file).unwrap();
        archive.file_names().map(str::to_string).collect()
    }

    #[test]
    fn the_image_gate_keeps_files_it_rejects_out_of_the_document() {
        let dir = tempfile::tempdir().unwrap();
        let pic = dir.path().join("pic.png");
        std::fs::write(&pic, one_pixel_png()).unwrap();
        let doc = dir.path().join("out.docx");
        let text = format!("![secret]({})", pic.display());
        let options = WriteOptions {
            image_gate: Some(|_| false),
            ..Default::default()
        };
        write_document_with_options(
            &doc,
            Some(&text),
            WriteMode::Create,
            None,
            "en-US",
            &options,
        )
        .unwrap();
        assert!(!zip_names(&doc).iter().any(|n| n.starts_with("word/media/")));
        assert!(zip_part(&doc, "word/document.xml")
            .unwrap()
            .contains("[image: secret"));
    }

    #[test]
    fn appending_images_keeps_earlier_media_and_drawing_ids() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pic.png"), one_pixel_png()).unwrap();
        let doc = dir.path().join("out.docx");
        write_document(
            &doc,
            Some("![one](pic.png)"),
            WriteMode::Create,
            None,
            "en-US",
        )
        .unwrap();
        write_document(
            &doc,
            Some("![two](pic.png)"),
            WriteMode::Append,
            None,
            "en-US",
        )
        .unwrap();
        let names = zip_names(&doc);
        assert!(
            names.iter().any(|n| n == "word/media/image1.png"),
            "{names:?}"
        );
        assert!(
            names.iter().any(|n| n == "word/media/image2.png"),
            "{names:?}"
        );
        let body = zip_part(&doc, "word/document.xml").unwrap();
        assert_eq!(body.matches(r#"<wp:docPr id="1""#).count(), 1, "{body}");
        assert_eq!(body.matches(r#"<wp:docPr id="2""#).count(), 1, "{body}");
        let ct = zip_part(&doc, "[Content_Types].xml").unwrap();
        assert_eq!(ct.matches(r#"Extension="png""#).count(), 1, "{ct}");
    }

    #[test]
    fn appending_footnotes_numbers_them_after_the_existing_ones() {
        let dir = tempfile::tempdir().unwrap();
        let doc = dir.path().join("out.docx");
        write_document(
            &doc,
            Some("First[^a]\n\n[^a]: one"),
            WriteMode::Create,
            None,
            "en-US",
        )
        .unwrap();
        write_document(
            &doc,
            Some("Second[^a]\n\n[^a]: two"),
            WriteMode::Append,
            None,
            "en-US",
        )
        .unwrap();
        let body = zip_part(&doc, "word/document.xml").unwrap();
        assert!(
            body.contains(r#"<w:footnoteReference w:id="1"/>"#),
            "{body}"
        );
        assert!(
            body.contains(r#"<w:footnoteReference w:id="2"/>"#),
            "{body}"
        );
        let notes = zip_part(&doc, "word/footnotes.xml").unwrap();
        assert!(notes.contains(r#"<w:footnote w:id="1">"#) && notes.contains(" one<"));
        assert!(notes.contains(r#"<w:footnote w:id="2">"#) && notes.contains(" two<"));
        let rels = zip_part(&doc, "word/_rels/document.xml.rels").unwrap();
        assert_eq!(rels.matches("relationships/footnotes").count(), 1, "{rels}");
        let ct = zip_part(&doc, "[Content_Types].xml").unwrap();
        assert_eq!(ct.matches("/word/footnotes.xml").count(), 1, "{ct}");
    }
}
