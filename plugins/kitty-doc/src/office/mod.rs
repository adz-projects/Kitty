//! Presentation and OpenDocument text → markdown (`.pptx`, `.odt`).
//!
//! Additive: library entry points for Bacon's source import. Both formats are
//! zip packages of XML parts; parts are read with the same per-entry size cap
//! as Word documents.

pub mod odt;
pub mod pptx;

use std::path::Path;

use crate::docx::{read_zip_entry, MAX_DOCX_ENTRIES};

pub(crate) type Archive = zip::ZipArchive<std::fs::File>;

pub(crate) fn open(path: &Path) -> Result<Archive, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let archive =
        zip::ZipArchive::new(file).map_err(|e| format!("not a valid document package: {e}"))?;
    if archive.len() > MAX_DOCX_ENTRIES {
        return Err(format!("package has too many entries ({})", archive.len()));
    }
    Ok(archive)
}

pub(crate) fn entry(archive: &mut Archive, name: &str) -> Option<Vec<u8>> {
    read_zip_entry(archive, name)
}

/// Resolve a relationship target against the directory of the part that
/// names it (`../notesSlides/n1.xml` from `ppt/slides/`).
pub(crate) fn resolve_target(base_dir: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut parts: Vec<&str> = base_dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in target.split('/') {
        match seg {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// `dc:title` from a metadata part, collapsed to one line.
pub(crate) fn meta_title(xml_bytes: &[u8]) -> Option<String> {
    let root = crate::markdown::xml::parse(xml_bytes);
    let t = root.find("title")?.text();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    (!t.is_empty()).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_resolve_relative_to_their_part() {
        assert_eq!(
            resolve_target("ppt/slides", "../notesSlides/notesSlide1.xml"),
            "ppt/notesSlides/notesSlide1.xml"
        );
        assert_eq!(
            resolve_target("ppt", "slides/slide2.xml"),
            "ppt/slides/slide2.xml"
        );
        assert_eq!(
            resolve_target("ppt", "/ppt/slides/s.xml"),
            "ppt/slides/s.xml"
        );
    }
}
