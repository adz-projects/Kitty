//! Turning a page's bytes into text in the encoding it was written in.
//!
//! Everything used to be read as UTF-8 (`from_utf8_lossy`), so a page in
//! windows-1252, Shift_JIS, GBK, EUC-KR or KOI8-R came back as replacement
//! characters. This follows the order a browser uses, simplified: a byte
//! order mark, then the `Content-Type` header's `charset`, then a `<meta>`
//! (or XML) declaration near the top, then UTF-8 if the bytes are valid
//! UTF-8, and windows-1252 — the web's legacy default — if they are not.

use std::sync::OnceLock;

use encoding_rs::{Encoding, UTF_8, WINDOWS_1252};

/// How far into a page a `<meta charset>` declaration is looked for. The
/// HTML spec's prescan reads 1024 bytes; real pages put a long `<head>`
/// ahead of it often enough that a little more is worth reading.
const SNIFF_BYTES: usize = 4096;

/// The `charset` parameter of a `Content-Type` header value, if any.
pub fn header_charset(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|param| {
        let (k, v) = param.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| v.trim().trim_matches(['"', '\'']).to_string())
            .filter(|v| !v.is_empty())
    })
}

/// Decode a page (or feed, or API reply) given its header charset.
pub fn decode_text(bytes: &[u8], header: Option<&str>) -> String {
    if let Some((enc, bom_len)) = Encoding::for_bom(bytes) {
        return enc
            .decode_without_bom_handling(&bytes[bom_len..])
            .0
            .into_owned();
    }
    let declared = header
        .and_then(|label| Encoding::for_label(label.trim().as_bytes()))
        .or_else(|| sniff(&bytes[..bytes.len().min(SNIFF_BYTES)]));
    let enc = match declared {
        Some(enc) => enc,
        None if std::str::from_utf8(bytes).is_ok() => UTF_8,
        None => WINDOWS_1252,
    };
    enc.decode_without_bom_handling(bytes).0.into_owned()
}

/// The encoding a `<meta>` tag or an XML declaration in `head` names. A
/// declared UTF-16 is read as UTF-8, as the HTML spec says: bytes that
/// reached an ASCII-compatible prescan cannot be UTF-16.
fn sniff(head: &[u8]) -> Option<&'static Encoding> {
    static META: OnceLock<regex::bytes::Regex> = OnceLock::new();
    static XML: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let meta = META.get_or_init(|| {
        regex::bytes::Regex::new(r#"(?i)<meta\b[^>]*?charset\s*=\s*["']?\s*([A-Za-z0-9_.:\-]+)"#)
            .expect("static regex")
    });
    let xml = XML.get_or_init(|| {
        regex::bytes::Regex::new(r#"(?i)^\s*<\?xml\b[^>]*?encoding\s*=\s*["']([A-Za-z0-9_.:\-]+)"#)
            .expect("static regex")
    });
    let label = meta
        .captures(head)
        .or_else(|| xml.captures(head))
        .and_then(|c| c.get(1))?;
    let enc = Encoding::for_label(label.as_bytes())?;
    Some(
        if enc == encoding_rs::UTF_16LE || enc == encoding_rs::UTF_16BE {
            UTF_8
        } else {
            enc
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_charset_is_read_from_the_content_type() {
        assert_eq!(
            header_charset("text/html; charset=Shift_JIS").as_deref(),
            Some("Shift_JIS")
        );
        assert_eq!(
            header_charset("text/html;charset=\"utf-8\"").as_deref(),
            Some("utf-8")
        );
        assert_eq!(header_charset("text/html"), None);
    }

    #[test]
    fn the_header_wins_over_a_meta_tag() {
        let (bytes, _, _) = encoding_rs::SHIFT_JIS.encode("<p>日本語</p>");
        let out = decode_text(&bytes, Some("shift_jis"));
        assert!(out.contains("日本語"), "{out}");
    }

    #[test]
    fn a_meta_declaration_is_honoured_without_a_header() {
        let mut bytes = b"<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=windows-1251\"></head><body>".to_vec();
        let (body, _, _) = encoding_rs::WINDOWS_1251.encode("Привет");
        bytes.extend_from_slice(&body);
        assert!(decode_text(&bytes, None).contains("Привет"));

        let mut bytes = b"<meta charset=gbk><p>".to_vec();
        let (body, _, _) = encoding_rs::GBK.encode("中文");
        bytes.extend_from_slice(&body);
        assert!(decode_text(&bytes, None).contains("中文"));
    }

    #[test]
    fn an_xml_declaration_counts_for_feeds() {
        let mut bytes = b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><rss><title>".to_vec();
        bytes.extend_from_slice(&[b'C', b'a', b'f', 0xE9]);
        assert!(decode_text(&bytes, None).contains("Café"));
    }

    #[test]
    fn undeclared_bytes_are_utf8_when_valid_and_windows_1252_when_not() {
        assert_eq!(decode_text("Café".as_bytes(), None), "Café");
        assert_eq!(decode_text(&[b'C', b'a', b'f', 0xE9], None), "Café");
    }

    #[test]
    fn a_bom_wins_and_is_stripped() {
        let bytes = [0xEF, 0xBB, 0xBF, b'h', b'i'];
        assert_eq!(decode_text(&bytes, Some("windows-1252")), "hi");
    }

    #[test]
    fn a_declared_utf16_is_read_as_utf8() {
        let bytes = "<meta charset=\"utf-16\"><p>é</p>".as_bytes();
        assert!(decode_text(bytes, None).contains('é'));
    }
}
