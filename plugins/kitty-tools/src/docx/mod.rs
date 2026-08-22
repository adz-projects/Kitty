pub mod read;
pub mod styles;
pub mod write;

use std::io::Read as _;
use std::path::Path;

use read::ParagraphInfo;

#[derive(Debug)]
pub enum DocxError {
    NotFound,
    /// Not a valid zip / missing required parts / malformed XML.
    Corrupt(String),
}

/// Satisfies `doc_store::ensure`'s `E: From<String>` bound. The only failure
/// the store itself raises before the extractor runs is being unable to stat
/// the file, which for a `.docx` read is indistinguishable from it not being
/// there — so it lands on `NotFound` and the caller's existing
/// `DOCX_NOT_FOUND` branch, rather than inventing a second error code for a
/// case the user cannot tell apart.
impl From<String> for DocxError {
    fn from(_detail: String) -> Self {
        DocxError::NotFound
    }
}

/// Per-part decompressed-size cap when reading a `.docx` archive — a zip
/// bomb (a tiny compressed entry that expands to gigabytes) must bail with a
/// corrupt-DOCUMENT error instead of exhausting process memory.
pub(crate) const MAX_DOCX_ENTRY_BYTES: u64 = 100 * 1024 * 1024;
/// Entry-count cap for append-mode part enumeration.
pub(crate) const MAX_DOCX_ENTRIES: usize = 4096;

/// Opens a `.docx`, resolves every paragraph's styleId to its style name via
/// `word/styles.xml`, and extracts paragraphs (including inside tables and
/// text boxes — see `read` module doc comment) in document order.
pub fn read_paragraphs(path: &Path) -> Result<Vec<ParagraphInfo>, DocxError> {
    if !path.exists() {
        return Err(DocxError::NotFound);
    }
    let file = std::fs::File::open(path).map_err(|e| DocxError::Corrupt(e.to_string()))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| DocxError::Corrupt(e.to_string()))?;
    if archive.len() > MAX_DOCX_ENTRIES {
        return Err(DocxError::Corrupt(format!(
            "archive has too many entries ({})",
            archive.len()
        )));
    }

    let document_xml = read_zip_entry(&mut archive, "word/document.xml")
        .ok_or_else(|| DocxError::Corrupt("missing word/document.xml".to_string()))?;
    let style_names = read_zip_entry(&mut archive, "word/styles.xml")
        .map(|xml| styles::parse_style_names(&xml))
        .unwrap_or_default();

    Ok(read::extract_paragraphs(&document_xml, &style_names))
}

fn read_zip_entry(archive: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Option<Vec<u8>> {
    let file = archive.by_name(name).ok()?;
    let mut buf = Vec::new();
    // Cap decompression (`take` + explicit size check) so a bombed entry is
    // treated as corrupt rather than materialized into memory.
    file.take(MAX_DOCX_ENTRY_BYTES + 1)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() as u64 > MAX_DOCX_ENTRY_BYTES {
        return None;
    }
    Some(buf)
}
