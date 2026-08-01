//! Resolves a paragraph's `w:pStyle` **styleId** to its human-readable
//! **name** via `word/styles.xml`.
//!
//! This is the base plan's flagship Word trap: python-docx's `p.style.name`
//! returns the *name* (`"Heading 1"`), which is what `lean_mcp.py`'s
//! `_read_docx_robust` and `word_read_outline` both regex/parse. A direct
//! XML scan only ever sees the **styleId** (commonly `"Heading1"`, no
//! space) on `<w:pStyle w:val="...">` — treating that as if it were the
//! name would make every heading-detection regex silently fail. This module
//! resolves styleId -> name once per document so callers can operate on the
//! name exactly like the original code did.

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use std::collections::HashMap;

/// styleId -> style name (e.g. `"Heading1"` -> `"Heading 1"`).
pub type StyleNames = HashMap<String, String>;

pub fn parse_style_names(xml: &[u8]) -> StyleNames {
    let mut map = HashMap::new();
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);

    let mut current_style_id: Option<String> = None;
    let mut current_name: Option<String> = None;
    let mut in_style = false;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = e.name();
                let local = local_name(name.as_ref());
                if local == b"style" {
                    in_style = true;
                    current_style_id = attr_value(&e, b"w:styleId");
                    current_name = None;
                } else if in_style && local == b"name" {
                    if let Some(val) = attr_value(&e, b"w:val") {
                        current_name = Some(val);
                    }
                }
            }
            Ok(Event::End(e)) => {
                if local_name(e.name().as_ref()) == b"style" {
                    if let (Some(id), Some(name)) = (current_style_id.take(), current_name.take()) {
                        map.insert(id, name);
                    }
                    in_style = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    map
}

fn local_name(qualified: &[u8]) -> &[u8] {
    match qualified.iter().position(|&b| b == b':') {
        Some(idx) => &qualified[idx + 1..],
        None => qualified,
    }
}

fn attr_value(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        if local_name(a.key.as_ref()) == local_name(key) {
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok().map(|v| v.into_owned())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STYLES_XML: &str = r#"<?xml version="1.0"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:style w:type="paragraph" w:styleId="Heading1">
    <w:name w:val="heading 1"/>
  </w:style>
  <w:style w:type="paragraph" w:styleId="Heading2">
    <w:name w:val="heading 2"/>
  </w:style>
  <w:style w:type="paragraph" w:styleId="Normal">
    <w:name w:val="Normal"/>
  </w:style>
</w:styles>"#;

    #[test]
    fn resolves_style_id_to_name() {
        let map = parse_style_names(STYLES_XML.as_bytes());
        assert_eq!(map.get("Heading1").map(String::as_str), Some("heading 1"));
        assert_eq!(map.get("Heading2").map(String::as_str), Some("heading 2"));
        assert_eq!(map.get("Normal").map(String::as_str), Some("Normal"));
    }
}
