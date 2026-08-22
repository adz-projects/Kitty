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
const MAX_STORED_DOCS: usize = 20;

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
fn id_for(resolved: &Path, unit: &str, len: u64, mtime_nanos: u128) -> String {
    // `unit` is in the key so two readers over the same bytes (a `.docx` read
    // as paragraphs, the same path read as lines) can't collide on one record
    // and serve each other's units.
    let key = format!(
        "{}\u{1f}{unit}\u{1f}{len}\u{1f}{mtime_nanos}",
        resolved.to_string_lossy()
    );
    format!("{:016x}", fnv1a(key.as_bytes()))
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
    serde_json::from_str(&raw).map_err(|e| LoadError::Unreadable(e.to_string()))
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
pub fn ensure<F, E>(resolved: &Path, unit: &str, extract: F) -> Result<(StoredDoc, bool), E>
where
    F: FnOnce() -> Result<Extraction, E>,
    E: From<String>,
{
    let (len, mtime_nanos) =
        fingerprint(resolved).ok_or_else(|| E::from("could not stat the document".to_string()))?;
    let document_id = id_for(resolved, unit, len, mtime_nanos);

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
        units: extraction.units,
        outline: extraction.outline,
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
    std::fs::write(&path, body).map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// One window of units, plus whether anything follows it. Clamps rather than
/// erroring on an offset past the end, so a caller walking `next_offset` to
/// completion gets an empty final page instead of a failure.
pub fn window(units: &[String], offset: usize, limit: usize) -> (Vec<String>, bool) {
    let page: Vec<String> = units.iter().skip(offset).take(limit).cloned().collect();
    let has_more = offset.saturating_add(page.len()) < units.len();
    (page, has_more)
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
        let first = id_for(&f, UNIT_LINE, len, mtime);
        assert_eq!(
            first,
            id_for(&f, UNIT_LINE, len, mtime),
            "same input, same id"
        );

        // A different fingerprint is a different document.
        assert_ne!(first, id_for(&f, UNIT_LINE, len + 1, mtime));
        assert_ne!(first, id_for(&f, UNIT_LINE, len, mtime + 1));
        // ...and so is the same bytes read as a different kind of unit.
        assert_ne!(first, id_for(&f, UNIT_PARAGRAPH, len, mtime));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn issued_ids_are_always_well_formed() {
        let dir = scratch("wellformed");
        let f = dir.join("b.txt");
        std::fs::write(&f, "x").unwrap();
        let (len, mtime) = fingerprint(&f).unwrap();
        assert!(is_well_formed_id(&id_for(&f, UNIT_PAGE, len, mtime)));
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
        let (first, persisted) = ensure::<_, String>(&f, UNIT_LINE, || {
            calls += 1;
            Ok(Extraction::new(vec!["1: hello".into()], vec![]))
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert!(persisted, "the record should have been written");
        assert_eq!(first.units, vec!["1: hello".to_string()]);

        // Second call: same fingerprint, so the extractor must not run again.
        let (second, _) = ensure::<_, String>(&f, UNIT_LINE, || {
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
        let (first, _) = ensure::<_, String>(&f, UNIT_LINE, || {
            Ok(Extraction::new(vec!["before".into()], vec![]))
        })
        .unwrap();

        // A same-length edit — length alone would not notice this one.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&f, "afterX").unwrap();
        let (second, _) = ensure::<_, String>(&f, UNIT_LINE, || {
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
        };
        let dir = scratch("short");
        let f = dir.join("e.pdf");
        std::fs::write(&f, "x").unwrap();
        let (doc, _) = ensure::<_, String>(&f, UNIT_PAGE, || Ok(e)).unwrap();
        assert!(doc.extraction_truncated);
        assert_eq!(doc.total_units, 600);
        assert_eq!(doc.stored_units(), 2);

        std::fs::remove_file(record_path(&doc.document_id)).ok();
        std::fs::remove_dir_all(&dir).ok();
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
        let err: String = ensure(&f, UNIT_LINE, || Err("boom".to_string())).unwrap_err();
        assert_eq!(err, "boom");
        std::fs::remove_dir_all(&dir).ok();
    }
}
