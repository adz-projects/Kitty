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

/// Opens a `.docx`, resolves every paragraph's styleId to its style name via
/// `word/styles.xml`, and extracts paragraphs (including inside tables and
/// text boxes — see `read` module doc comment) in document order.
pub fn read_paragraphs(path: &Path) -> Result<Vec<ParagraphInfo>, DocxError> {
    if !path.exists() {
        return Err(DocxError::NotFound);
    }
    let file = std::fs::File::open(path).map_err(|e| DocxError::Corrupt(e.to_string()))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| DocxError::Corrupt(e.to_string()))?;

    let document_xml = read_zip_entry(&mut archive, "word/document.xml")
        .ok_or_else(|| DocxError::Corrupt("missing word/document.xml".to_string()))?;
    let style_names = read_zip_entry(&mut archive, "word/styles.xml")
        .map(|xml| styles::parse_style_names(&xml))
        .unwrap_or_default();

    Ok(read::extract_paragraphs(&document_xml, &style_names))
}

fn read_zip_entry(archive: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Option<Vec<u8>> {
    let mut file = archive.by_name(name).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    Some(buf)
}
