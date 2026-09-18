//! PDF read tools — Rust port of `kitty_docs_web.py`'s `lean_pdf_read_text`
//! and `lean_pdf_read_outline`, originally on `lopdf` alone, now extracting
//! text with `pdf-extract` (pure Rust, on the same lopdf).
//!
//! Tool names, JSON envelope, error codes and pagination/query contracts are
//! kept compatible with the Python original.
//!
//! ## Why not lopdf's own `extract_text`
//!
//! It was the extractor here until it was found to return a blank page for
//! nearly every real PDF — which led models to report that text documents
//! were "just images". `tests/pdf_real_world.rs` pins each failure. In lopdf
//! 0.34 one font on a page with no usable encoding (an Identity-H font with
//! no ToUnicode, a ToUnicode CMap with multi-character ligature entries — both
//! routine in Word, browser and LaTeX output) failed the *whole page*, and the
//! error was swallowed into an empty string. It also never entered Form
//! XObjects and ignored the `'`/`"` show operators. `pdf-extract` handles all
//! of those; lopdf's `extract_text` is kept only as a per-page fallback.
//!
//! `pdf-extract` panics on some malformed input (`unwrap`/`expect`/`todo!`
//! on font data), so each page runs under `catch_unwind` on its own: one bad
//! page costs that page, never the document. On Android this crate is linked
//! into the app, which is why `src-tauri`'s release profile must not set
//! `panic = "abort"`.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

use lopdf::content::Content;
use lopdf::{Dictionary, Document, Object, ObjectId};
use serde_json::{json, Value};

use crate::doc_store::{self, Extraction};
use crate::envelope::{error_response, success_response};
use crate::paths::{path_within_allowed, resolve};
use crate::query_filter::filter_by_query;

/// Hard cap on pages extracted in one call when no `end_page` is given — a
/// 10,000-page PDF must not balloon the payload. The response byte budget
/// (`doc_store::RESPONSE_BUDGET_BYTES`) usually stops a read well before this.
const PDF_MAX_PAGES: u32 = 100;
/// Per-page text cap — an attack/malformed page can otherwise yield an
/// effectively unbounded extracted string.
const PDF_MAX_PAGE_CHARS: usize = 50_000;
/// Hard cap on the PDF file size, checked before `lopdf::Document::load`
/// (audit #120): `load` reads the whole file into memory, so a giant file
/// must be rejected at the door. Same pattern as `fs.rs`'s `MAX_FILE_BYTES`.
const PDF_MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// The extractor-version salt for `doc_store` ids (see `doc_store::id_for`).
/// Bump it whenever a change to extraction should replace records already on
/// disk — the lopdf-era records were blank, and must never be served again.
const PDF_EXTRACTOR: &str = "pdf-extract-0.12";
/// How deep `scan_content` follows Form XObjects nested in Form XObjects.
const MAX_FORM_DEPTH: u8 = 8;

/// Placed in the page body itself, not only in the response metadata, because
/// `lean_doc_read_chunk` serves page bodies without that metadata — the note
/// has to travel with the page.
const NOTE_IMAGE_ONLY: &str =
    "[No text layer: this page is an image, most likely a scan. Its text cannot be read without OCR.]";
const NOTE_FAILED: &str = "[This page has text that could not be extracted — its font carries no \
     usable character mapping. It is not an image.]";

fn open(path: &Path) -> Result<Document, lopdf::Error> {
    // lopdf 0.42 decrypts during `load` whenever the empty user password
    // opens the file — the owner-password-only, permissions-restricted PDFs
    // that every viewer opens without a prompt, and that 0.34 made this tool
    // refuse as "password protected".
    let mut doc = Document::load(path)?;
    repair_to_unicode_cmaps(&mut doc);
    Ok(doc)
}

/// Rewrite the one malformation in ToUnicode CMaps that both extractors
/// reject outright: a destination above U+FFFF written as its bare code point
/// (`<10780>`, five hex digits) instead of as a UTF-16 surrogate pair
/// (`<D801DF80>`). MuPDF — and so every PDF PyMuPDF writes with an embedded
/// font that has such glyphs, Arial included — emits exactly this. It is only
/// a handful of entries in the whole map, but lopdf then discards the entire
/// map and decodes glyph ids as if they were characters (garbage), and
/// `pdf-extract`'s CMap parser panics.
///
/// Five hex digits is never a valid token here — every destination is whole
/// UTF-16 units, so an even digit count — which is what makes the rewrite
/// safe: nothing correct can match it.
fn repair_to_unicode_cmaps(doc: &mut Document) {
    use std::sync::OnceLock;
    static BARE_ASTRAL: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let re = BARE_ASTRAL
        .get_or_init(|| regex::bytes::Regex::new(r"<([0-9A-Fa-f]{5})>").expect("static regex"));

    let cmap_ids: Vec<ObjectId> = doc
        .objects
        .values()
        .filter_map(|o| o.as_dict().ok())
        .filter(|d| d.has_type(b"Font"))
        .filter_map(|d| d.get(b"ToUnicode").and_then(Object::as_reference).ok())
        .collect();
    for id in cmap_ids {
        let Some(Object::Stream(stream)) = doc.objects.get_mut(&id) else {
            continue;
        };
        let Ok(plain) = stream.get_plain_content() else {
            continue;
        };
        if !re.is_match(&plain) {
            continue;
        }
        let fixed = re.replace_all(&plain, |caps: &regex::bytes::Captures| {
            let hex = std::str::from_utf8(&caps[1]).unwrap_or("0");
            let cp = u32::from_str_radix(hex, 16).unwrap_or(0xFFFD);
            let mut units = [0u16; 2];
            let encoded = char::from_u32(cp)
                .unwrap_or(char::REPLACEMENT_CHARACTER)
                .encode_utf16(&mut units);
            let mut out = String::from("<");
            for unit in encoded.iter() {
                out.push_str(&format!("{unit:04X}"));
            }
            out.push('>');
            out.into_bytes()
        });
        stream.set_plain_content(fixed.into_owned());
    }
}

/// Why an extraction couldn't be produced. `Stat` exists only to satisfy
/// `doc_store::ensure`'s `E: From<String>` bound — that is the one failure the
/// store itself can raise before the extractor runs.
enum PdfError {
    Corrupt(String),
    Encrypted,
    Stat(String),
}

impl From<String> for PdfError {
    fn from(detail: String) -> Self {
        PdfError::Stat(detail)
    }
}

impl PdfError {
    fn into_response(self, resolved: &Path) -> String {
        let detail = Some(resolved.to_string_lossy().into_owned());
        let detail = detail.as_deref();
        match self {
            PdfError::Corrupt(e) => error_response(
                "PDF_CORRUPT",
                &format!("Cannot parse PDF: {e}"),
                detail,
                // Desktop viewers silently rebuild a damaged cross-reference
                // table, so "it opens fine for me" is expected here, and not a
                // reason to retry this tool.
                Some(
                    "The file's internal structure is damaged. PDF viewers repair this on open, \
                     so it may still display normally; re-saving or printing it to a new PDF \
                     produces a readable copy.",
                ),
            ),
            PdfError::Encrypted => error_response(
                "PDF_ENCRYPTED",
                "PDF is password protected",
                detail,
                Some(
                    "It cannot be opened without its password. (PDFs that only restrict \
                     printing or copying open without one and are read normally.)",
                ),
            ),
            PdfError::Stat(e) => error_response(
                "PDF_READ_ERROR",
                &format!("Cannot read PDF: {e}"),
                detail,
                None,
            ),
        }
    }
}

/// One page's text, from whichever extractor read more of it. `None` only
/// when both came back with no text at all.
///
/// `pdf-extract` is the primary: measured against PyMuPDF on a set of
/// published PDFs it recovered 99–100% of words intact on every one, where
/// lopdf 0.42 ranged from 44% to 99% (it glues or splits words when spacing is
/// done by positioning). But `pdf-extract` silently skips the `'`/`"` show
/// operators, which lopdf handles, so lopdf wins a page when it finds clearly
/// more text — by a margin, so that near-ties go to the better word splitter.
fn page_text(doc: &Document, pno: u32) -> Option<String> {
    let primary = catch_unwind(AssertUnwindSafe(|| {
        let mut s = String::new();
        {
            let mut out = pdf_extract::PlainTextOutput::new(&mut s);
            pdf_extract::output_doc_page(doc, &mut out, pno).ok()?;
        }
        Some(s)
    }))
    .ok()
    .flatten();
    let fallback = catch_unwind(AssertUnwindSafe(|| doc.extract_text(&[pno]).ok()))
        .ok()
        .flatten();

    let alnum = |s: &Option<String>| {
        s.as_deref()
            .map_or(0, |t| t.chars().filter(|c| c.is_alphanumeric()).count())
    };
    let (p, f) = (alnum(&primary), alnum(&fallback));
    let chosen = if f > p + p / 8 + 4 { fallback } else { primary };
    chosen.map(|t| normalize_text(&t)).filter(|t| !t.is_empty())
}

/// Make extracted text read and search the way it looks on the page.
///
/// Typographic ligatures come through as their presentation-form code points —
/// `oﬃce` with U+FB03 — so a search for "office" missed every occurrence set
/// in a font with ligatures, which is most body text from Word, browsers and
/// LaTeX. They are expanded here. Runs of spaces (layout gaps, not content)
/// collapse to one, lines lose trailing space, and blank-line runs collapse to
/// a single blank line.
fn normalize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in text.lines() {
        let mut clean = String::with_capacity(line.len());
        let mut prev_space = false;
        for c in line.chars() {
            let expanded = match c {
                '\u{FB00}' => "ff",
                '\u{FB01}' => "fi",
                '\u{FB02}' => "fl",
                '\u{FB03}' => "ffi",
                '\u{FB04}' => "ffl",
                '\u{FB05}' | '\u{FB06}' => "st",
                _ => {
                    let space = c == ' ' || c == '\u{A0}' || c == '\t';
                    if space {
                        if !prev_space {
                            clean.push(' ');
                        }
                    } else {
                        clean.push(c);
                    }
                    prev_space = space;
                    continue;
                }
            };
            clean.push_str(expanded);
            prev_space = false;
        }
        let clean = clean.trim_end();
        if clean.is_empty() {
            blank_run += 1;
            if blank_run > 1 || out.is_empty() {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(clean);
        out.push('\n');
    }
    out.trim().to_string()
}

/// Why a page produced no text. Decided from the page's own content, so the
/// model is told which it is instead of being left to guess — a blank result
/// was being read as "this PDF is a scanned image" even when it was text.
#[derive(Debug, PartialEq)]
enum EmptyPage {
    /// Nothing drawn as text and nothing drawn as an image — a genuinely blank
    /// page, or one whose lettering is vector outlines.
    Blank,
    /// Images and no text operators: a scan.
    ImageOnly,
    /// Text operators are present, but nothing could be decoded from them.
    Failed,
}

#[derive(Default)]
struct PageScan {
    text_ops: bool,
    images: bool,
}

fn classify_empty(doc: &Document, page_id: ObjectId) -> EmptyPage {
    let mut scan = PageScan::default();
    let resources = page_resources(doc, page_id);
    if let Ok(content) = doc.get_page_content(page_id) {
        scan_content(doc, &content, &resources, 0, &mut scan);
    }
    match (scan.text_ops, scan.images) {
        (true, _) => EmptyPage::Failed,
        (false, true) => EmptyPage::ImageOnly,
        (false, false) => EmptyPage::Blank,
    }
}

/// The page's resource dictionaries, own first, then inherited.
fn page_resources(doc: &Document, page_id: ObjectId) -> Vec<&Dictionary> {
    let Ok((own, inherited)) = doc.get_page_resources(page_id) else {
        return Vec::new();
    };
    own.into_iter()
        .chain(
            inherited
                .into_iter()
                .filter_map(|id| doc.get_dictionary(id).ok()),
        )
        .collect()
}

fn scan_content(
    doc: &Document,
    content: &[u8],
    resources: &[&Dictionary],
    depth: u8,
    scan: &mut PageScan,
) {
    let Ok(content) = Content::decode(content) else {
        return;
    };
    for op in &content.operations {
        match op.operator.as_str() {
            "Tj" | "TJ" | "'" | "\"" => scan.text_ops = true,
            // An inline image.
            "BI" | "ID" | "EI" => scan.images = true,
            "Do" => {
                let Some(name) = op.operands.first().and_then(|o| o.as_name().ok()) else {
                    continue;
                };
                let Some(xobject) = find_xobject(doc, resources, name) else {
                    continue;
                };
                match xobject.dict.get(b"Subtype").and_then(Object::as_name) {
                    Ok(b"Image") => scan.images = true,
                    Ok(b"Form") if depth < MAX_FORM_DEPTH => {
                        let Ok(inner) = xobject.decompressed_content() else {
                            continue;
                        };
                        // A form's own /Resources win; without them it draws
                        // with its parent's.
                        let own = xobject
                            .dict
                            .get(b"Resources")
                            .and_then(|r| doc.dereference(r))
                            .and_then(|(_, r)| r.as_dict())
                            .ok();
                        let scoped: Vec<&Dictionary> =
                            own.into_iter().chain(resources.iter().copied()).collect();
                        scan_content(doc, &inner, &scoped, depth + 1, scan);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        if scan.text_ops {
            // Text operators decide the verdict on their own; stop early.
            return;
        }
    }
}

fn find_xobject<'a>(
    doc: &'a Document,
    resources: &[&'a Dictionary],
    name: &[u8],
) -> Option<&'a lopdf::Stream> {
    resources.iter().find_map(|res| {
        let (_, xobjects) = doc.dereference(res.get(b"XObject").ok()?).ok()?;
        let (_, obj) = doc
            .dereference(xobjects.as_dict().ok()?.get(name).ok()?)
            .ok()?;
        obj.as_stream().ok()
    })
}

/// Parse the PDF **once** and extract every page, plus its table of contents.
///
/// This is what `doc_store::ensure` runs on a cache miss. It deliberately
/// ignores the caller's page range: the old code extracted only the requested
/// window and reparsed the file for the next one, so reading a long PDF end to
/// end reparsed it once per chunk. Bounded by `doc_store::MAX_TOTAL_CHARS`
/// rather than by `PDF_MAX_PAGES` — the latter is now purely a cap on how much
/// one *response* carries, not on how much is ever read.
fn extract_pages(resolved: &Path) -> Result<Extraction, PdfError> {
    let doc = open(resolved).map_err(|e| match e {
        lopdf::Error::InvalidPassword => PdfError::Encrypted,
        e => PdfError::Corrupt(e.to_string()),
    })?;
    // lopdf removes `/Encrypt` from the trailer once the empty password has
    // opened the file, so one still there means it did not: this PDF really
    // does need a password. Checked as any `/Encrypt`, not `is_encrypted()`,
    // which only recognises an indirect reference — MuPDF writes the
    // dictionary inline, and lopdf then loads such a file with *no objects at
    // all*, which read as a successful, zero-page, empty document.
    if doc.trailer.get(b"Encrypt").is_ok() {
        return Err(PdfError::Encrypted);
    }

    // lopdf's get_toc already flattens the outline tree into
    // { level, title, page } — the same triple PyMuPDF's get_toc produced.
    let outline: Vec<Value> = match doc.get_toc() {
        Ok(toc) => toc
            .toc
            .into_iter()
            .map(|o| json!({ "level": o.level, "title": o.title, "page": o.page }))
            .collect(),
        Err(_) => Vec::new(),
    };

    let pages = doc.get_pages();
    let total_pages = pages.len();
    let mut units = Vec::with_capacity(total_pages);
    let mut content_capped = false;
    let mut chars = 0usize;
    let mut image_only: Vec<u32> = Vec::new();
    let mut failed: Vec<u32> = Vec::new();
    for (&pno, &page_id) in &pages {
        let page_text = match page_text(&doc, pno) {
            Some(text) => {
                let mut text = text.trim().to_string();
                if text.chars().count() > PDF_MAX_PAGE_CHARS {
                    content_capped = true;
                    text = truncate_chars(&text, PDF_MAX_PAGE_CHARS);
                }
                text
            }
            None => match classify_empty(&doc, page_id) {
                EmptyPage::Blank => String::new(),
                EmptyPage::ImageOnly => {
                    image_only.push(pno);
                    NOTE_IMAGE_ONLY.to_string()
                }
                EmptyPage::Failed => {
                    failed.push(pno);
                    NOTE_FAILED.to_string()
                }
            },
        };
        let unit = format!("--- Page {pno} ---\n{page_text}");
        chars = chars.saturating_add(unit.chars().count());
        units.push(unit);
        if chars >= doc_store::MAX_TOTAL_CHARS {
            break;
        }
    }

    let mut diagnostics = serde_json::Map::new();
    if !image_only.is_empty() {
        diagnostics.insert("pages_image_only".into(), json!(image_only));
    }
    if !failed.is_empty() {
        diagnostics.insert("pages_failed".into(), json!(failed));
    }

    Ok(Extraction {
        units,
        outline,
        total_units: total_pages,
        content_capped,
        diagnostics,
    })
}

/// A plain-language line for the response message, naming the pages that came
/// back without text and why. `None` when every page had text.
fn diagnostics_note(doc: &doc_store::StoredDoc) -> Option<String> {
    let list = |key: &str| -> Vec<u64> {
        doc.diagnostics
            .get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default()
    };
    let fmt = |pages: &[u64]| {
        let shown: Vec<String> = pages.iter().take(20).map(u64::to_string).collect();
        let more = pages.len().saturating_sub(20);
        if more > 0 {
            format!("{} and {more} more", shown.join(", "))
        } else {
            shown.join(", ")
        }
    };
    let image_only = list("pages_image_only");
    let failed = list("pages_failed");
    let mut parts = Vec::new();
    if !image_only.is_empty() {
        if image_only.len() == doc.total_units {
            parts.push(
                "Every page is an image with no text layer — this PDF is a scan, and its text \
                 cannot be read without OCR."
                    .to_string(),
            );
        } else {
            parts.push(format!(
                "Page(s) {} have no text layer (images, most likely scans); every other page is \
                 real text.",
                fmt(&image_only)
            ));
        }
    }
    if !failed.is_empty() {
        parts.push(format!(
            "Page(s) {} contain text that could not be extracted (a font with no character \
             mapping) — they are not images.",
            fmt(&failed)
        ));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Shared entry: every PDF tool goes through the cache, so whichever one the
/// model calls first pays the single parse and the rest are slices.
fn cached_pdf(resolved: &Path) -> Result<doc_store::StoredDoc, String> {
    match doc_store::ensure(resolved, doc_store::UNIT_PAGE, PDF_EXTRACTOR, || {
        extract_pages(resolved)
    }) {
        Ok((doc, _persisted)) => Ok(doc),
        Err(e) => Err(e.into_response(resolved)),
    }
}

/// Single-stat existence + size probe: one `metadata()` answers "does it
/// exist" and "is it over the cap" together instead of `exists()` plus a
/// second stat.
fn stat_len(resolved: &Path) -> Option<u64> {
    std::fs::metadata(resolved).map(|m| m.len()).ok()
}

fn too_large(resolved: &Path) -> String {
    error_response(
        "PDF_TOO_LARGE",
        &format!(
            "File is larger than the {} byte read limit",
            PDF_MAX_FILE_BYTES
        ),
        Some(&resolved.to_string_lossy()),
        Some("Split the PDF, or read a page range from a smaller copy."),
    )
}

/// Home boundary shared by both PDF tools — defense-in-depth before any
/// filesystem access (the daemon is the primary gate).
fn outside_home(resolved: &Path) -> Option<String> {
    if path_within_allowed(resolved) {
        None
    } else {
        Some(error_response(
            "PATH_OUTSIDE_HOME",
            "Path is outside the directories this session may access",
            Some(&resolved.to_string_lossy()),
            Some(&crate::paths::allowed_roots_hint()),
        ))
    }
}

/// Truncates `s` to at most `max_chars` characters, appending a `…` marker
/// so truncation is visible in the payload. Slices at a character boundary
/// without materializing the whole string as a `Vec<char>` first.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    // `nth(max_chars)` is the first char *past* the budget; `None` means the
    // string holds at most `max_chars` characters and is returned as-is.
    let Some((idx, _)) = s.char_indices().nth(max_chars) else {
        return s.to_string();
    };
    format!("{}…", &s[..idx])
}

pub fn pdf_read_text(
    path: &str,
    start_page: Option<u32>,
    end_page: Option<u32>,
    query: Option<&str>,
    offset: usize,
) -> String {
    let resolved = resolve(path);
    if let Some(err) = outside_home(&resolved) {
        return err;
    }
    let len = match stat_len(&resolved) {
        Some(len) => len,
        None => {
            return error_response(
                "PDF_NOT_FOUND",
                "PDF does not exist",
                Some(&resolved.to_string_lossy()),
                None,
            );
        }
    };
    if len > PDF_MAX_FILE_BYTES {
        return too_large(&resolved);
    }

    let s_page = start_page.unwrap_or(1).max(1);
    // Checked before the parse: an inverted range is a caller mistake that
    // shouldn't first cost reading and parsing a file of up to 64 MB. And the
    // clamping arithmetic further down turns it into something worse than an
    // error — `end_page` gets floored at `start_page - 1`, so asking for pages
    // 5–2 returned an empty list reported as `end_page: 4`, a range nobody
    // asked for with no indication anything was wrong.
    if let Some(end) = end_page {
        if end < s_page {
            return error_response(
                "PDF_BAD_RANGE",
                &format!("end_page ({end}) is before start_page ({s_page})"),
                Some(&resolved.to_string_lossy()),
                Some("Pass end_page greater than or equal to start_page, or omit it."),
            );
        }
    }

    // One parse, cached — see `extract_pages`. Every branch below slices the
    // cached pages rather than touching the PDF again.
    let doc = match cached_pdf(&resolved) {
        Ok(d) => d,
        Err(envelope) => return envelope,
    };

    let total_pages = doc.total_units as u32;
    let held_pages = doc.stored_units() as u32;
    // Both the caller's `end_page` and the hard cap bound the *response*; the
    // extraction above already covers the whole document.
    let end_requested = end_page
        .map(|e| e.min(held_pages))
        .unwrap_or(held_pages)
        .max(s_page.saturating_sub(1));
    let capped_end = s_page.saturating_add(PDF_MAX_PAGES - 1).min(held_pages);
    let e_page = end_requested.min(capped_end);
    let truncated = end_requested > e_page || doc.extraction_truncated;

    let extracted_pages: &[String] = if s_page <= e_page {
        &doc.units[(s_page - 1) as usize..e_page as usize]
    } else {
        &[]
    };
    let note = diagnostics_note(&doc);

    if let Some(q) = query.filter(|q| !q.trim().is_empty()) {
        let result = filter_by_query(extracted_pages, Some(q), 50, offset)
            .fit_to_budget(offset, doc_store::RESPONSE_BUDGET_BYTES);
        let message = join_messages([
            result
                .no_match
                .then(|| format!("No direct matches for query '{q}'. Showing top section.")),
            note,
        ]);
        let mut meta = serde_json::Map::new();
        meta.insert("document_id".into(), json!(doc.document_id));
        meta.insert("start_page".into(), json!(s_page));
        meta.insert("end_page".into(), json!(e_page));
        meta.insert("filtered_by_query".into(), json!(q));
        meta.insert("total_matches".into(), json!(result.total_matches));
        meta.insert("offset".into(), json!(offset));
        if let Some(next) = result.next_offset {
            meta.insert("next_offset".into(), json!(next));
        }
        meta.extend(doc.diagnostics.clone());
        let any_truncated = truncated || result.truncated;
        return success_response(
            json!(result.items),
            message.as_deref(),
            any_truncated,
            Some(Value::Object(meta)),
        );
    }

    let (outline, outline_bytes) = doc_store::outline_for_window(&doc.outline, s_page == 1);
    let fitted = doc_store::fit_to_budget(
        extracted_pages,
        doc_store::RESPONSE_BUDGET_BYTES.saturating_sub(outline_bytes),
    );
    // Where this response actually ends, which the byte budget can bring in
    // well short of `e_page`.
    let served_end = if extracted_pages.is_empty() {
        e_page
    } else {
        s_page - 1 + fitted.items.len() as u32
    };
    let truncated = truncated || fitted.stopped_early || fitted.item_capped;
    let has_more = served_end < held_pages;
    // The handle is the point: say plainly that the rest of the document is
    // one `lean_doc_read_chunk` away rather than leaving the model to guess
    // that re-calling with a new page range is cheap now.
    let position = if doc.extraction_truncated {
        Some(format!(
            "Document is too large to extract in full: pages 1-{held_pages} of {total_pages} are \
             available. Read the rest with lean_doc_read_chunk using document_id, or narrow the \
             page range."
        ))
    } else if fitted.stopped_early {
        Some(format!(
            "Showing pages {s_page}-{served_end} of {total_pages} — stopped there to stay within \
             the response size limit. Read on with lean_doc_read_chunk (document_id, offset \
             {served_end}) or search it with lean_doc_search."
        ))
    } else if has_more {
        Some(format!(
            "Showing pages {s_page}-{served_end} of {total_pages}. The whole document is already \
             extracted and cached — read on with lean_doc_read_chunk (document_id, offset \
             {served_end}) or search it with lean_doc_search."
        ))
    } else {
        None
    };
    let capped = fitted
        .item_capped
        .then(|| format!("Page {s_page} is longer than one response can carry and was cut short."));
    let message = join_messages([position, capped, note]);

    let mut meta = serde_json::Map::new();
    meta.insert("document_id".into(), json!(doc.document_id));
    meta.insert("unit".into(), json!(doc.unit));
    meta.insert("start_page".into(), json!(s_page));
    meta.insert("end_page".into(), json!(served_end));
    meta.insert("total_pages".into(), json!(total_pages));
    meta.insert("pages_available".into(), json!(held_pages));
    meta.insert("has_more".into(), json!(has_more));
    if has_more {
        meta.insert("next_offset".into(), json!(served_end));
    }
    match outline {
        Some(outline) => {
            meta.insert("outline".into(), outline);
        }
        None if !doc.outline.is_empty() => {
            meta.insert("outline_available".into(), json!(true));
        }
        None => {}
    }
    meta.extend(doc.diagnostics.clone());
    success_response(
        json!(fitted.items),
        message.as_deref(),
        truncated,
        Some(Value::Object(meta)),
    )
}

/// The non-empty parts of a response message, as one message.
fn join_messages<const N: usize>(parts: [Option<String>; N]) -> Option<String> {
    let parts: Vec<String> = parts.into_iter().flatten().collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

pub fn pdf_read_outline(path: &str) -> String {
    let resolved = resolve(path);
    if let Some(err) = outside_home(&resolved) {
        return err;
    }
    let len = match stat_len(&resolved) {
        Some(len) => len,
        None => {
            return error_response(
                "PDF_NOT_FOUND",
                "PDF does not exist",
                Some(&resolved.to_string_lossy()),
                None,
            );
        }
    };
    if len > PDF_MAX_FILE_BYTES {
        return too_large(&resolved);
    }

    // Same cache as `pdf_read_text`: the outline is extracted alongside the
    // pages, so whichever tool the model reaches for first pays the one parse
    // and the other is free. Returning the `document_id` here too means an
    // outline-first read loop can go straight to `lean_doc_read_chunk`.
    let doc = match cached_pdf(&resolved) {
        Ok(d) => d,
        Err(envelope) => return envelope,
    };

    success_response(
        json!(doc.outline),
        None,
        false,
        Some(json!({
            "document_id": doc.document_id,
            "unit": doc.unit,
            "total_pages": doc.total_units,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_reports_pdf_not_found() {
        // Inside home (temp dir) so the boundary passes through to the
        // not-found error path.
        let dir = std::env::temp_dir().join(format!("kt-pdf-missing-{}", std::process::id()));
        let missing = dir.join("does-not-exist.pdf");
        let out = pdf_read_text(missing.to_str().unwrap(), None, None, None, 0);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "error");
        assert_eq!(v["error_code"], "PDF_NOT_FOUND");
    }

    #[test]
    fn missing_outline_file_reports_pdf_not_found() {
        let dir = std::env::temp_dir().join(format!("kt-pdf-missing-o-{}", std::process::id()));
        let missing = dir.join("does-not-exist.pdf");
        let out = pdf_read_outline(missing.to_str().unwrap());
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error_code"], "PDF_NOT_FOUND");
    }

    #[test]
    fn outside_home_path_is_rejected() {
        #[cfg(windows)]
        let p = "C:\\Windows\\System32\\calc.exe";
        #[cfg(not(windows))]
        let p = "/etc/passwd";
        let out = pdf_read_text(p, None, None, None, 0);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error_code"], "PATH_OUTSIDE_HOME");
    }

    #[test]
    fn truncate_chars_limited_and_keeps_marker() {
        let s = "abcde";
        assert_eq!(truncate_chars(s, 3), "abc…");
        assert_eq!(truncate_chars(s, 5), "abcde");
    }

    #[test]
    fn oversized_pdf_is_rejected_before_loading() {
        // Audit #120: `Document::load` reads the whole file into memory; the
        // size gate must fire first (a 64 MiB write exercises the metadata
        // check without a valid PDF ever being parsed).
        let dir = std::env::temp_dir().join(format!("kt-pdf-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("big.pdf");
        std::fs::write(&f, vec![b'x'; (PDF_MAX_FILE_BYTES + 1) as usize]).unwrap();

        let out = pdf_read_text(f.to_str().unwrap(), None, None, None, 0);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error_code"], "PDF_TOO_LARGE");

        let out = pdf_read_outline(f.to_str().unwrap());
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error_code"], "PDF_TOO_LARGE");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// An inverted range used to be absorbed by the clamping arithmetic: pages
    /// 5-2 came back as an empty list reported with `end_page: 4`, a range
    /// nobody asked for and no indication anything was wrong.
    #[test]
    fn an_inverted_page_range_is_rejected_rather_than_silently_reshaped() {
        let dir = std::env::temp_dir().join(format!("kt-pdf-range-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.pdf");
        // Deliberately not a valid PDF: the range check runs before the
        // parse, so this asserts the ordering as well as the verdict.
        std::fs::write(&f, b"%PDF-1.4 not really a pdf").unwrap();

        let out = pdf_read_text(f.to_str().unwrap(), Some(5), Some(2), None, 0);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error_code"], "PDF_BAD_RANGE", "{v}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
