//! Extract-once document cache — the store behind `document_id`.
//!
//! Every paged reader in this crate used to redo its *entire* extraction on
//! each call and then throw away everything outside the requested window:
//! `lean_pdf_read_text` ran a full `lopdf::Document::load` per 100-page chunk,
//! `lean_word_read_text` unzipped and XML-parsed the whole `.docx` per 200
//! paragraphs. Reading a 600-page PDF end to end parsed that PDF six times.
//!
//! So extraction happens once, into a cache keyed by the file's identity
//! (`path`, `len`, `mtime`), and every subsequent read is served from it. The
//! id is *derived* from that key rather than random, which is what makes the
//! cache transparent: calling `lean_pdf_read_text` twice on an unchanged file
//! yields the same `document_id` and a hit, while editing the file changes its
//! fingerprint and therefore its id, so a stale extraction can never be served
//! for content that has moved on. Nothing needs invalidating.
//!
//! Layout mirrors `kitty-web`'s search offload (`search.rs`'s
//! `write_offload`/`prune_old_offloads`) deliberately — same store-under-the-
//! cache-dir shape, same newest-N pruning, same "the handle is only advertised
//! if the write actually succeeded" rule. This crate keeps its own copy rather
//! than sharing a module, following the duplication convention the sibling
//! crates already document (`paths.rs`, `envelope.rs`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tools::cache_dir;

/// What one addressable unit of a document is, per source kind. Carried in the
/// stored record and echoed in every response so a caller reading by `offset`
/// knows what it is counting.
pub const UNIT_PAGE: &str = "page";
pub const UNIT_PARAGRAPH: &str = "paragraph";
pub const UNIT_LINE: &str = "line";

/// Most recent extractions to keep. Same bound, and the same reasoning, as
/// `kitty-web`'s `MAX_OFFLOAD_FILES`: enough that a working set of documents
/// stays warm across a conversation, small enough that the cache directory
/// can't grow without limit.
///
/// Was 20, which parallel specialists outran: three delegates each opening a
/// handful of documents pruned a handle another was still reading in a loop,
/// and its next `lean_doc_read_chunk` failed with `DOC_NOT_FOUND` part-way
/// through. Pruning is also least-recently-*used* now — see `touch`.
const MAX_STORED_DOCS: usize = 100;

/// How much document text one read response may carry, in serialized JSON
/// bytes.
///
/// The daemon cuts every tool result at 100 KB (`MAX_TOOL_OUTPUT_BYTES` in
/// `BigTinyV2/daemon/src/mcp/tools.rs`), and it cuts blind: the envelope
/// serializes `data` before `message` and `metadata`, so an oversized read
/// lost exactly the parts that say how to continue — the `document_id`,
/// `has_more`, `next_offset` — and left the model holding a torn JSON
/// fragment. Every default page size here (100 PDF pages, 200 chunk units, 200
/// paragraphs) could overflow it on an ordinary document. So each reader stops
/// at this budget instead and says where it stopped. The ~20 KB of headroom is
/// for the pretty-printed envelope and metadata.
pub const RESPONSE_BUDGET_BYTES: usize = 80 * 1024;

/// An outline larger than this is left out of a read response rather than
/// spent against its budget; `lean_pdf_read_outline` still returns it whole.
pub const OUTLINE_INLINE_MAX_BYTES: usize = 16 * 1024;

/// Ceiling on the total extracted text held for one document.
///
/// Extraction is no longer bounded by the per-call page cap — the whole point
/// is to do it once — so it needs its own bound. Without this, a 10,000-page
/// PDF at the 50,000-char-per-page cap could try to materialize ~500 MB. When
/// the ceiling is hit, extraction stops and the record records how far it got:
/// `total_units` stays the document's real count while `units` holds fewer, so
/// the shortfall is visible rather than silently indistinguishable from a
/// document that simply ended.
pub const MAX_TOTAL_CHARS: usize = 8 * 1024 * 1024;

/// A document that has been extracted and cached. `units` are stored in their
/// final rendered form (page bodies already carry their `--- Page N ---`
/// header, text lines already carry their `N: ` prefix), so serving a chunk is
/// a slice and the chunk tool cannot drift from the reader that produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredDoc {
    pub document_id: String,
    pub source_path: String,
    /// One of `UNIT_PAGE`/`UNIT_PARAGRAPH`/`UNIT_LINE`.
    pub unit: String,
    /// The document's real unit count, which is `units.len()` unless
    /// extraction hit `MAX_TOTAL_CHARS`.
    pub total_units: usize,
    pub units: Vec<String>,
    /// Structure, where the source kind has any: `{level, title, page}` for a
    /// PDF's table of contents, `{level, title, offset}` for a Word document's
    /// headings. Empty for kinds with no notion of one (plain text).
    pub outline: Vec<Value>,
    /// True when this record does not hold the document's complete text —
    /// either `MAX_TOTAL_CHARS` stopped it short of `total_units`, or an
    /// individual unit's content was capped.
    pub extraction_truncated: bool,
    /// Storage format: 0 = legacy units stored pre-numbered (`"N: line"` baked
    /// in at extraction), 1 = raw lines numbered on serve. Absent in records
    /// written before the change, which therefore read back as 0.
    #[serde(default)]
    pub format_version: u32,
    /// Extractor findings worth passing on with every read — for a PDF, which
    /// pages have no text layer and which failed to extract. Merged into the
    /// response metadata as-is.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub diagnostics: serde_json::Map<String, Value>,
    len: u64,
    mtime_nanos: u128,
}

impl StoredDoc {
    /// Units actually held, which is what `offset` indexes into.
    pub fn stored_units(&self) -> usize {
        self.units.len()
    }
}

/// Why a `document_id` could not be served. Each maps to a distinct error code
/// at the tool boundary.
#[derive(Debug)]
pub enum LoadError {
    /// The id isn't the shape this module issues — rejected before it is ever
    /// joined onto a path.
    Malformed,
    /// No record under that id: never created, or pruned since.
    NotFound,
    /// The record exists but couldn't be read or parsed.
    Unreadable(String),
}

fn store_dir() -> PathBuf {
    cache_dir().join("documents")
}

fn record_path(document_id: &str) -> PathBuf {
    store_dir().join(format!("doc-{document_id}.json"))
}

/// Ids are exactly 16 lowercase hex characters, so this is both a format check
/// and a complete traversal guard: the accepted alphabet contains no path
/// separator, no `.`, and no `:` (an NTFS alternate data stream), which is the
/// same class of hole `cache.rs::rejects_traversal` exists to close. Validating
/// by allowlist rather than by denylist is what makes that guarantee total
/// rather than a list of the escapes anyone thought of.
fn is_well_formed_id(document_id: &str) -> bool {
    document_id.len() == 16
        && document_id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
}

/// FNV-1a (64-bit). A local hash rather than a new `sha2` dependency for a
/// frozen binary that is already 17 MB: this names a local cache entry, and
/// nothing trusts the id on its own — `ensure` re-checks the stored `len` and
/// `mtime_nanos` against the file before it will serve a hit, so a collision
/// costs a re-extraction, not a wrong answer.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

/// `(len, mtime)` for a file, or `None` when it can't be stat'd. Both are part
/// of the identity: length alone misses an in-place edit that preserves size.
fn fingerprint(resolved: &Path) -> Option<(u64, u128)> {
    let meta = std::fs::metadata(resolved).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some((meta.len(), mtime))
}

/// The id for a file's current contents. Same file, unchanged → same id.
///
/// `extractor` names the version of the code that produced the record. The
/// fingerprint only says the *file* is unchanged; a record extracted by an
/// older, broken extractor is just as fresh by that measure. Without this,
/// fixing the PDF extractor would have changed nothing for any PDF a user had
/// already tried: its blank pages were cached on disk under an id the new
/// code would compute identically. Bumping the string moves every record of
/// that kind to a new id, and the old ones age out through pruning. Empty for
/// readers that have never needed a bump, which keeps their ids unchanged.
fn id_for(resolved: &Path, unit: &str, extractor: &str, len: u64, mtime_nanos: u128) -> String {
    // `unit` is in the key so two readers over the same bytes (a `.docx` read
    // as paragraphs, the same path read as lines) can't collide on one record
    // and serve each other's units.
    let mut key = format!(
        "{}\u{1f}{unit}\u{1f}{len}\u{1f}{mtime_nanos}",
        resolved.to_string_lossy()
    );
    if !extractor.is_empty() {
        key.push('\u{1f}');
        key.push_str(extractor);
    }
    format!("{:016x}", fnv1a(key.as_bytes()))
}

/// Mark a record as just used, so `prune_old_records` — which keeps the newest
/// by mtime — keeps the documents being *read*, not only those most recently
/// extracted. Best-effort: failing to touch costs nothing but an earlier
/// eviction.
fn touch(path: &Path) {
    if let Ok(f) = std::fs::File::options().write(true).open(path) {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
}

/// Drop all but the newest `MAX_STORED_DOCS - 1` records, leaving room for the
/// one about to be written. Best-effort: a record that won't delete is left
/// alone rather than failing the extraction that triggered the prune.
fn prune_old_records() {
    let dir = store_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("doc-"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    for (_, stale) in files.into_iter().skip(MAX_STORED_DOCS.saturating_sub(1)) {
        let _ = std::fs::remove_file(stale);
    }
}

/// Load a record by id, verifying nothing about the source file — the caller
/// is asking for a stored extraction, not for the file's current state. This
/// is what lets a chunk read keep working on a handle whose source has since
/// been edited or deleted, rather than failing halfway through a read loop.
pub fn load(document_id: &str) -> Result<StoredDoc, LoadError> {
    if !is_well_formed_id(document_id) {
        return Err(LoadError::Malformed);
    }
    let path = record_path(document_id);
    if !path.exists() {
        return Err(LoadError::NotFound);
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| LoadError::Unreadable(e.to_string()))?;
    let doc = serde_json::from_str(&raw).map_err(|e| LoadError::Unreadable(e.to_string()))?;
    touch(&path);
    Ok(doc)
}

/// The extracted form of a document: its units, its outline, and the real unit
/// count when that is larger than the units returned (i.e. the extractor cut
/// itself short at `MAX_TOTAL_CHARS`).
pub struct Extraction {
    pub units: Vec<String>,
    pub outline: Vec<Value>,
    /// The document's real unit count. Larger than `units.len()` when the
    /// extractor stopped early at `MAX_TOTAL_CHARS`.
    pub total_units: usize,
    /// Some individual unit's own content was capped (a single pathological
    /// PDF page past `PDF_MAX_PAGE_CHARS`, say). Distinct from stopping early:
    /// every unit is present, but one of them is short.
    pub content_capped: bool,
    /// See `StoredDoc::diagnostics`.
    pub diagnostics: serde_json::Map<String, Value>,
}

impl Extraction {
    /// A complete extraction — every unit present, none of them capped.
    pub fn new(units: Vec<String>, outline: Vec<Value>) -> Self {
        let total_units = units.len();
        Self {
            units,
            outline,
            total_units,
            content_capped: false,
            diagnostics: serde_json::Map::new(),
        }
    }
}

/// Get `resolved`'s extraction, running `extract` only on a miss.
///
/// A hit requires the stored `len`/`mtime_nanos` to still match the file on
/// disk. The id already encodes both, so a mismatch means an FNV collision
/// rather than a stale entry — rare, and handled by re-extracting over the top
/// instead of trusting it.
///
/// A record that cannot be written is reported through the returned
/// `StoredDoc` being served from memory anyway: extraction succeeded, so the
/// caller's own read is answered in full. What the caller must not do is
/// advertise the `document_id` — see `persisted`.
///
/// `extractor` is the extractor-version salt described on `id_for`.
pub fn ensure<F, E>(
    resolved: &Path,
    unit: &str,
    extractor: &str,
    extract: F,
) -> Result<(StoredDoc, bool), E>
where
    F: FnOnce() -> Result<Extraction, E>,
    E: From<String>,
{
    let (len, mtime_nanos) =
        fingerprint(resolved).ok_or_else(|| E::from("could not stat the document".to_string()))?;
    let document_id = id_for(resolved, unit, extractor, len, mtime_nanos);

    if let Ok(hit) = load(&document_id) {
        if hit.len == len && hit.mtime_nanos == mtime_nanos {
            return Ok((hit, true));
        }
    }

    let extraction = extract()?;
    let doc = StoredDoc {
        document_id,
        source_path: resolved.to_string_lossy().into_owned(),
        unit: unit.to_string(),
        total_units: extraction.total_units,
        extraction_truncated: extraction.total_units > extraction.units.len()
            || extraction.content_capped,
        format_version: STORED_FORMAT_VERSION,
        units: extraction.units,
        outline: extraction.outline,
        diagnostics: extraction.diagnostics,
        len,
        mtime_nanos,
    };
    let persisted = write_record(&doc).is_ok();
    Ok((doc, persisted))
}

fn write_record(doc: &StoredDoc) -> Result<(), String> {
    let dir = store_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    prune_old_records();
    let path = record_path(&doc.document_id);
    let body = serde_json::to_string(doc).map_err(|e| e.to_string())?;
    // Atomic but not fsync'd: a torn record is useless while a merely lost
    // one is just re-extracted. Buffered inside `write_atomic`.
    write_atomic(&path, body.as_bytes())
        .map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// One window of units as a borrow, plus whether anything follows it. Clamps
/// rather than erroring on an offset past the end, so a caller walking
/// `next_offset` to completion gets an empty final page instead of a failure.
/// Prefer this over `window`: it serves the page straight into the response
/// without cloning every string twice (once into a `Vec`, once into JSON).
pub fn window_slice(units: &[String], offset: usize, limit: usize) -> (&[String], bool) {
    let start = offset.min(units.len());
    let end = start.saturating_add(limit).min(units.len());
    (&units[start..end], end < units.len())
}

/// The format new records are written in: raw lines, numbered on serve.
pub const STORED_FORMAT_VERSION: u32 = 1;

/// Display forms of `doc.units[start..end]` (clamped), numbering only the
/// served slice when the record stores raw lines. Legacy records (version 0)
/// already carry their `"N: "` prefixes and pass through untouched, so a
/// cache written by an older build keeps serving byte-identical output.
pub fn display_range(doc: &StoredDoc, start: usize, end: usize) -> Vec<String> {
    let len = doc.units.len();
    let start = start.min(len);
    let mut end = end.min(len);
    if end < start {
        end = start;
    }
    if doc.unit == UNIT_LINE && doc.format_version >= STORED_FORMAT_VERSION {
        doc.units[start..end]
            .iter()
            .enumerate()
            .map(|(k, l)| format!("{}: {}", start + k + 1, l))
            .collect()
    } else {
        doc.units[start..end].to_vec()
    }
}

/// Display forms of every unit. Used by whole-document scans (keyword
/// search), which score the same numbered text the old pre-numbered records
/// carried; windowed reads should use `display_window` to number only the
/// served slice.
pub fn display_all(doc: &StoredDoc) -> Vec<String> {
    display_range(doc, 0, doc.units.len())
}

/// Display forms of one window plus whether anything follows it — the
/// numbered equivalent of `window` for records that store raw lines.
pub fn display_window(doc: &StoredDoc, offset: usize, limit: usize) -> (Vec<String>, bool) {
    let (_, has_more) = window_slice(&doc.units, offset, limit);
    let start = offset.min(doc.units.len());
    let end = start.saturating_add(limit).min(doc.units.len());
    (display_range(doc, start, end), has_more)
}

/// Fast same-dir temp-file + rename write without `fsync`: atomic (a reader
/// sees the whole old file or the whole new one, never a torn one) but not
/// durable — a power loss right after the rename may lose the write. That is
/// the intended trade for caches and tool outputs; the scratchpad keeps its
/// `sync_all` write. The caller ensures the parent directory exists.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tmp".to_string());
    let tmp = path.with_file_name(format!(".{stem}.tmp-{}-{n}", std::process::id()));

    let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
    f.write_all(bytes)?;
    f.flush()?;
    drop(f);
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// One window of units, plus whether anything follows it. Clamps rather than
/// erroring on an offset past the end, so a caller walking `next_offset` to
/// completion gets an empty final page instead of a failure.
pub fn window(units: &[String], offset: usize, limit: usize) -> (Vec<String>, bool) {
    let (page, has_more) = window_slice(units, offset, limit);
    (page.to_vec(), has_more)
}

/// The outline to attach to a read response, and what it costs against the
/// response budget.
///
/// Only on the first window of a document: the outline used to ride along on
/// *every* page of a paged read, so a long document with a large table of
/// contents paid for it again on each chunk. And not at all when it is large
/// enough to crowd out the text — `outline_available` then tells the caller it
/// exists and where to get it.
pub fn outline_for_window(outline: &[Value], first_window: bool) -> (Option<Value>, usize) {
    if !first_window {
        return (None, 0);
    }
    let value = Value::Array(outline.to_vec());
    let size = serde_json::to_string(&value).map(|s| s.len()).unwrap_or(0);
    if size > OUTLINE_INLINE_MAX_BYTES {
        return (None, 0);
    }
    (Some(value), size)
}

/// What survived `fit_to_budget`.
pub struct Fitted {
    pub items: Vec<String>,
    /// Fewer items were kept than offered, because the budget ran out.
    pub stopped_early: bool,
    /// The first item alone was over budget and was cut short.
    pub item_capped: bool,
}

/// Keep items, in order, while their serialized size fits `budget` bytes.
///
/// Measured as JSON string literals, since that is what the daemon's cap
/// counts: a page of newlines and quotes costs more on the wire than its UTF-8
/// length, and CJK text costs three bytes a character. The first item is
/// always kept — a response that returns nothing and says "read on" would
/// loop forever — and is cut to fit if it alone is too large.
pub fn fit_to_budget<S: AsRef<str>>(items: &[S], budget: usize) -> Fitted {
    let mut out = Vec::new();
    let mut used = 0usize;
    for item in items {
        let item = item.as_ref();
        // + the separator and pretty-print indent each array element costs.
        let cost = json_len(item) + 8;
        if used + cost > budget {
            if out.is_empty() {
                out.push(cap_json_len(item, budget));
                return Fitted {
                    stopped_early: items.len() > 1,
                    items: out,
                    item_capped: true,
                };
            }
            return Fitted {
                items: out,
                stopped_early: true,
                item_capped: false,
            };
        }
        used += cost;
        out.push(item.to_string());
    }
    Fitted {
        items: out,
        stopped_early: false,
        item_capped: false,
    }
}

/// Serialized length of `s` as a JSON string literal, quotes included.
pub fn json_len(s: &str) -> usize {
    serde_json::to_string(s).map(|j| j.len()).unwrap_or(s.len())
}

/// `s` shortened, at a character boundary, until its JSON form fits `budget`,
/// with a visible marker. Halving from the byte length converges in a few
/// steps even for escape-heavy text.
fn cap_json_len(s: &str, budget: usize) -> String {
    const MARKER: &str = "… [cut to fit the response limit]";
    let room = budget.saturating_sub(MARKER.len() + 16);
    let mut end = room.min(s.len());
    loop {
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 || json_len(&s[..end]) <= room {
            return format!("{}{MARKER}", &s[..end]);
        }
        end /= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kt-docstore-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ids_are_stable_for_unchanged_files_and_move_when_content_does() {
        let dir = scratch("id");
        let f = dir.join("a.txt");
        std::fs::write(&f, "one").unwrap();
        let (len, mtime) = fingerprint(&f).unwrap();
        let first = id_for(&f, UNIT_LINE, "", len, mtime);
        assert_eq!(
            first,
            id_for(&f, UNIT_LINE, "", len, mtime),
            "same input, same id"
        );

        // A different fingerprint is a different document.
        assert_ne!(first, id_for(&f, UNIT_LINE, "", len + 1, mtime));
        assert_ne!(first, id_for(&f, UNIT_LINE, "", len, mtime + 1));
        // ...and so is the same bytes read as a different kind of unit.
        assert_ne!(first, id_for(&f, UNIT_PARAGRAPH, "", len, mtime));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn issued_ids_are_always_well_formed() {
        let dir = scratch("wellformed");
        let f = dir.join("b.txt");
        std::fs::write(&f, "x").unwrap();
        let (len, mtime) = fingerprint(&f).unwrap();
        assert!(is_well_formed_id(&id_for(&f, UNIT_PAGE, "", len, mtime)));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The id alphabet is the traversal guard — an id is joined straight onto
    /// the store directory, so anything that isn't 16 lowercase hex digits has
    /// to be refused before it becomes a path (the hole `cache.rs` documents).
    #[test]
    fn malformed_ids_are_refused_before_becoming_a_path() {
        for bad in [
            "../../../../etc/passwd",
            "..\\..\\windows\\system32",
            "doc:stream",
            "ABCDEF0123456789", // uppercase is not the issued shape
            "short",
            "0123456789abcdef0", // 17
            "0123456789abcdeg",  // not hex
            "",
        ] {
            assert!(!is_well_formed_id(bad), "{bad} must be refused");
            assert!(
                matches!(load(bad), Err(LoadError::Malformed)),
                "{bad} must fail as malformed, not reach the filesystem"
            );
        }
    }

    #[test]
    fn a_miss_extracts_once_and_the_next_call_is_a_hit() {
        let dir = scratch("hit");
        let f = dir.join("c.txt");
        std::fs::write(&f, "hello").unwrap();

        let mut calls = 0;
        let (first, persisted) = ensure::<_, String>(&f, UNIT_LINE, "", || {
            calls += 1;
            Ok(Extraction::new(vec!["1: hello".into()], vec![]))
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert!(persisted, "the record should have been written");
        assert_eq!(first.units, vec!["1: hello".to_string()]);

        // Second call: same fingerprint, so the extractor must not run again.
        let (second, _) = ensure::<_, String>(&f, UNIT_LINE, "", || {
            calls += 1;
            Ok(Extraction::new(vec!["SHOULD NOT RUN".into()], vec![]))
        })
        .unwrap();
        assert_eq!(calls, 1, "an unchanged file must not be re-extracted");
        assert_eq!(second.document_id, first.document_id);
        assert_eq!(second.units, first.units);

        std::fs::remove_file(record_path(&first.document_id)).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The whole point of fingerprinting: an edited file must not keep serving
    /// the extraction of its previous contents.
    #[test]
    fn editing_the_file_produces_a_new_id_and_re_extracts() {
        let dir = scratch("edit");
        let f = dir.join("d.txt");
        std::fs::write(&f, "before").unwrap();
        let (first, _) = ensure::<_, String>(&f, UNIT_LINE, "", || {
            Ok(Extraction::new(vec!["before".into()], vec![]))
        })
        .unwrap();

        // A same-length edit — length alone would not notice this one.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&f, "afterX").unwrap();
        let (second, _) = ensure::<_, String>(&f, UNIT_LINE, "", || {
            Ok(Extraction::new(vec!["afterX".into()], vec![]))
        })
        .unwrap();

        assert_ne!(
            second.document_id, first.document_id,
            "edited content must not reuse the previous id"
        );
        assert_eq!(second.units, vec!["afterX".to_string()]);

        std::fs::remove_file(record_path(&first.document_id)).ok();
        std::fs::remove_file(record_path(&second.document_id)).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_short_extraction_records_the_real_total() {
        let e = Extraction {
            units: vec!["a".into(), "b".into()],
            outline: vec![],
            total_units: 600,
            content_capped: false,
            diagnostics: serde_json::Map::new(),
        };
        let dir = scratch("short");
        let f = dir.join("e.pdf");
        std::fs::write(&f, "x").unwrap();
        let (doc, _) = ensure::<_, String>(&f, UNIT_PAGE, "", || Ok(e)).unwrap();
        assert!(doc.extraction_truncated);
        assert_eq!(doc.total_units, 600);
        assert_eq!(doc.stored_units(), 2);

        std::fs::remove_file(record_path(&doc.document_id)).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Raw-line records number on serve; legacy records (written before the
    /// change, already carrying `"N: "` prefixes) pass through untouched so a
    /// stale cache keeps serving byte-identical output.
    #[test]
    fn display_numbers_raw_lines_and_passes_legacy_through() {
        let raw = StoredDoc {
            document_id: "0123456789abcdef".into(),
            source_path: "x".into(),
            unit: UNIT_LINE.into(),
            total_units: 2,
            units: vec!["alpha".into(), "beta".into()],
            outline: vec![],
            extraction_truncated: false,
            format_version: STORED_FORMAT_VERSION,
            diagnostics: serde_json::Map::new(),
            len: 0,
            mtime_nanos: 0,
        };
        assert_eq!(
            display_all(&raw),
            vec!["1: alpha".to_string(), "2: beta".to_string()]
        );
        assert_eq!(
            display_window(&raw, 1, 5),
            (vec!["2: beta".to_string()], false)
        );

        let legacy = StoredDoc {
            units: vec!["1: alpha".into(), "2: beta".into()],
            format_version: 0,
            ..raw.clone()
        };
        // Legacy units are already numbered: serve verbatim, never double up.
        assert_eq!(
            display_all(&legacy),
            vec!["1: alpha".to_string(), "2: beta".to_string()]
        );
        assert_eq!(
            display_window(&legacy, 0, 10),
            (vec!["1: alpha".to_string(), "2: beta".to_string()], false)
        );
    }

    #[test]
    fn window_clamps_past_the_end_instead_of_erroring() {
        let units: Vec<String> = (0..5).map(|i| i.to_string()).collect();

        let (page, has_more) = window(&units, 0, 2);
        assert_eq!(page, vec!["0".to_string(), "1".to_string()]);
        assert!(has_more);

        let (page, has_more) = window(&units, 3, 2);
        assert_eq!(page, vec!["3".to_string(), "4".to_string()]);
        assert!(!has_more, "the last window must not claim more follows");

        let (page, has_more) = window(&units, 99, 2);
        assert!(page.is_empty());
        assert!(!has_more);
    }

    #[test]
    fn extraction_failure_propagates_rather_than_caching_an_empty_document() {
        let dir = scratch("fail");
        let f = dir.join("f.txt");
        std::fs::write(&f, "x").unwrap();
        let err: String = ensure(&f, UNIT_LINE, "", || Err("boom".to_string())).unwrap_err();
        assert_eq!(err, "boom");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Bumping the extractor salt must move a document to a new id, so a
    /// record written by a broken extractor is never served to the fixed one.
    #[test]
    fn an_extractor_bump_is_a_different_document() {
        let dir = scratch("salt");
        let f = dir.join("g.pdf");
        std::fs::write(&f, "x").unwrap();
        let (len, mtime) = fingerprint(&f).unwrap();
        let unsalted = id_for(&f, UNIT_PAGE, "", len, mtime);
        let v1 = id_for(&f, UNIT_PAGE, "v1", len, mtime);
        assert_ne!(unsalted, v1);
        assert_ne!(v1, id_for(&f, UNIT_PAGE, "v2", len, mtime));

        let (old, _) = ensure::<_, String>(&f, UNIT_PAGE, "", || {
            Ok(Extraction::new(vec!["".into()], vec![]))
        })
        .unwrap();
        let (new, _) = ensure::<_, String>(&f, UNIT_PAGE, "v1", || {
            Ok(Extraction::new(vec!["real text".into()], vec![]))
        })
        .unwrap();
        assert_eq!(
            new.units,
            vec!["real text".to_string()],
            "the stale record was served"
        );

        std::fs::remove_file(record_path(&old.document_id)).ok();
        std::fs::remove_file(record_path(&new.document_id)).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A read refreshes the record, so pruning evicts what is idle rather than
    /// what happened to be extracted first.
    #[test]
    fn loading_a_record_marks_it_recently_used() {
        let dir = scratch("touch");
        let f = dir.join("h.txt");
        std::fs::write(&f, "x").unwrap();
        let (doc, _) = ensure::<_, String>(&f, UNIT_LINE, "", || {
            Ok(Extraction::new(vec!["x".into()], vec![]))
        })
        .unwrap();
        let path = record_path(&doc.document_id);
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();

        load(&doc.document_id).unwrap();
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(
            after > old + std::time::Duration::from_secs(60),
            "load did not touch"
        );

        std::fs::remove_file(path).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fit_to_budget_stops_at_whole_items_and_always_keeps_one() {
        let items: Vec<String> = (0..10).map(|_| "a".repeat(100)).collect();
        let f = fit_to_budget(&items, 350);
        assert_eq!(f.items.len(), 3);
        assert!(f.stopped_early);
        assert!(!f.item_capped);

        let f = fit_to_budget(&items, 100_000);
        assert_eq!(f.items.len(), 10);
        assert!(!f.stopped_early);

        // One item bigger than the whole budget is cut, not dropped.
        let huge = vec!["\"quoted\"\n".repeat(10_000)];
        let f = fit_to_budget(&huge, 1_000);
        assert_eq!(f.items.len(), 1);
        assert!(f.item_capped);
        assert!(json_len(&f.items[0]) <= 1_000, "{}", json_len(&f.items[0]));
        assert!(f.items[0].ends_with("[cut to fit the response limit]"));
    }

    /// Budget is JSON bytes, not characters: CJK is three bytes a character.
    #[test]
    fn fit_to_budget_counts_multibyte_text_by_its_encoded_size() {
        let items: Vec<String> = (0..10).map(|_| "漢".repeat(100)).collect();
        let f = fit_to_budget(&items, 1_000);
        assert_eq!(f.items.len(), 3, "300 bytes each");
    }
}
