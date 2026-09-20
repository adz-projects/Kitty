//! Envelope-free document → plain text, for the memorabilia ingest path.
//!
//! The BigTiny daemon links this crate as a library and calls
//! [`extract_document_text`] to turn a user-attached file into text for the
//! factual-memory engine, reusing the same per-format extractors (and their
//! extract-once cache) that back the MCP tools — rather than re-implementing
//! PDF/Word/Excel parsing. Images and other media are unsupported by design;
//! the caller logs and skips an `Err`.

use std::path::Path;

use crate::docx;
use crate::tools::{excel, fs, pdf};

/// Extract a document's plain text by absolute path. Dispatches on the file
/// extension. `Err` for an unsupported type (including images/media) or an
/// unreadable file — the caller (the daemon's turn-end harvester) logs and
/// skips it. No `resolve`/path-allow gating: the daemon ingests files the user
/// themselves attached, not paths the model chose.
pub fn extract_document_text(path: &Path) -> Result<String, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "pdf" => pdf::extract_pdf_text(path),
        "docx" => docx::read_paragraphs(path)
            .map(|paras| {
                paras
                    .into_iter()
                    .map(|p| p.text)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            // DocxError is Debug-only.
            .map_err(|e| format!("{e:?}")),
        "xlsx" | "xlsm" | "xls" | "ods" => excel::extract_excel_text(path),
        // Plain-text families. csv/tsv are also spreadsheet-openable, but a
        // direct decode keeps their raw delimiters intact for chunking.
        "txt" | "text" | "md" | "markdown" | "rst" | "csv" | "tsv" | "json"
        | "jsonl" | "ndjson" | "xml" | "yaml" | "yml" | "toml" | "ini" | "log"
        | "srt" | "vtt" | "html" | "htm" => fs::extract_text_file(path),
        "" => Err(format!("no file extension on {}", path.display())),
        other => Err(format!("unsupported document type: .{other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("kitty-extract-{nanos}-{name}"));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn text_and_csv_extract_their_contents() {
        let txt = tmp("note.txt", b"line one\nline two");
        assert_eq!(
            extract_document_text(&txt).unwrap(),
            "line one\nline two"
        );
        let _ = std::fs::remove_file(&txt);

        let csv = tmp("data.csv", b"a,b,c\n1,2,3");
        assert_eq!(extract_document_text(&csv).unwrap(), "a,b,c\n1,2,3");
        let _ = std::fs::remove_file(&csv);
    }

    #[test]
    fn media_and_unknown_types_are_errors() {
        // Images/media are out of scope — the caller skips the Err.
        let png = tmp("pic.png", &[0x89, b'P', b'N', b'G']);
        assert!(extract_document_text(&png).is_err());
        let _ = std::fs::remove_file(&png);

        let zip = tmp("bundle.zip", b"PK\x03\x04");
        assert!(extract_document_text(&zip).is_err());
        let _ = std::fs::remove_file(&zip);
    }

    #[test]
    fn missing_extension_is_an_error() {
        let p = std::env::temp_dir().join("kitty-extract-noext");
        assert!(extract_document_text(&p).is_err());
    }
}
