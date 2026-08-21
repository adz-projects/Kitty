//! Direct XML scan over `word/document.xml` — this is what
//! `lean_mcp.py`'s `_read_docx_robust` did as a *fallback*, but here it's
//! the **only** read path (Track B fix: eliminates the "standard vs
//! robust" gate entirely, since the bug was the gate itself — `if not
//! standard_elements: fall back to robust` blinds itself to a document
//! whose content is entirely inside tables/text boxes, because
//! `doc.paragraphs` (body-only) isn't empty, it's just wrong).
//!
//! Scanning the raw XML naturally reaches paragraphs inside `<w:tbl>` table
//! cells and text boxes, so there is no separate "does it need the
//! fallback" decision to get wrong.
//!
//! Deliberate simplification vs. a literal port: a paragraph nested inside
//! another (a text box's `<w:txbxContent><w:p>` sitting inside its anchor
//! paragraph's run) is folded into its outer paragraph's text rather than
//! also emitted as its own separate entry. The original Python's `.//w:p`
//! XPath emits *both* — the anchor paragraph (whose own `.//w:t` XPath
//! already descends into the nested text box and picks up its text) *and*
//! the nested paragraph again on its own — silently duplicating that text.
//! Not documented in the base plan, but a real behavior difference; noted
//! here since a future golden-comparison against `lean_mcp.py` would show it.

use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

use super::styles::StyleNames;

#[derive(Debug, Clone, PartialEq)]
pub struct ParagraphInfo {
    pub text: String,
    pub heading_level: Option<u32>,
}

#[derive(Default)]
struct RawParagraph {
    text: String,
    style_id: Option<String>,
    outline_lvl: Option<u32>,
    is_bold: bool,
    max_sz: Option<u32>,
    captured_own_ppr: bool,
}

pub fn extract_paragraphs(document_xml: &[u8], style_names: &StyleNames) -> Vec<ParagraphInfo> {
    let raws = extract_raw_paragraphs(document_xml);
    raws.into_iter()
        .filter_map(|raw| {
            let text = raw.text.trim().to_string();
            if text.is_empty() {
                return None;
            }
            let heading_level = compute_heading_level(&raw, style_names);
            Some(ParagraphInfo {
                text,
                heading_level,
            })
        })
        .collect()
}

fn compute_heading_level(raw: &RawParagraph, style_names: &StyleNames) -> Option<u32> {
    if let Some(style_id) = &raw.style_id {
        let name = style_names
            .get(style_id)
            .cloned()
            .unwrap_or_else(|| style_id.clone());
        if name.to_lowercase().contains("heading") {
            if let Some(level) = first_digit_run(&name) {
                return Some(level);
            }
        }
    }
    if let Some(lvl) = raw.outline_lvl {
        return Some(lvl + 1);
    }
    if raw.is_bold {
        if let Some(sz) = raw.max_sz {
            if sz >= 28 {
                return Some(1);
            } else if sz >= 24 {
                return Some(2);
            } else if sz >= 20 {
                return Some(3);
            } else if sz >= 18 {
                return Some(4);
            }
        }
    }
    None
}

/// Mirrors `re.findall(r"\d+", style_name)[0]` — the first contiguous run
/// of digits anywhere in the string, parsed as an integer.
fn first_digit_run(s: &str) -> Option<u32> {
    let mut digits = String::new();
    let mut started = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            started = true;
        } else if started {
            break;
        }
    }
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

fn local_name(qualified: &[u8]) -> &[u8] {
    match qualified.iter().position(|&b| b == b':') {
        Some(idx) => &qualified[idx + 1..],
        None => qualified,
    }
}

fn attr_value(e: &BytesStart, key: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        if local_name(a.key.as_ref()) == local_name(key) {
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        } else {
            None
        }
    })
}

fn extract_raw_paragraphs(document_xml: &[u8]) -> Vec<RawParagraph> {
    let mut reader = Reader::from_reader(document_xml);
    reader.config_mut().trim_text(false);

    let mut results = Vec::new();
    let mut current: Option<RawParagraph> = None;
    let mut paragraph_depth: usize = 0;
    let mut in_own_ppr = false;
    let mut in_rpr = false;
    // Only text inside a `w:t` is real paragraph content — field codes
    // (`w:instrText`), tracked-change deletions (`w:delText`), ruby glosses
    // (`w:rt`, whose runs still contain `w:t` elements) and inter-element
    // whitespace all emit text events too, but a reader never sees them.
    // This matches the Python port's `.//w:t` scan for non-ruby content.
    let mut in_t = false;
    let mut ruby_depth: usize = 0;
    // `mc:Fallback` content is dropped so a text box's AlternateContent
    // doesn't contribute its paragraph text twice (once via `mc:Choice`,
    // once via `mc:Fallback`) — Word/python-docx effectively prefer the
    // Choice branch; we approximate that by unconditionally skipping
    // Fallback.
    let mut fallback_depth: Option<usize> = None;
    let mut depth: usize = 0;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                depth += 1;
                let local = local_name(e.name().as_ref()).to_vec();

                if fallback_depth.is_none() && local == b"Fallback" {
                    fallback_depth = Some(depth);
                }
                if fallback_depth.is_some() {
                    buf.clear();
                    continue;
                }

                if local == b"p" {
                    if paragraph_depth == 0 {
                        current = Some(RawParagraph::default());
                    }
                    paragraph_depth += 1;
                } else if current.is_some() {
                    if local == b"t" {
                        in_t = true;
                    } else if local == b"tab" {
                        if let Some(p) = current.as_mut() {
                            p.text.push('\t');
                        }
                    } else if local == b"br" {
                        if let Some(p) = current.as_mut() {
                            p.text.push('\n');
                        }
                    } else if local == b"rt" {
                        ruby_depth += 1;
                    } else if local == b"pPr" && paragraph_depth == 1 {
                        in_own_ppr = true;
                    } else if in_own_ppr && local == b"pStyle" {
                        if let Some(p) = current.as_mut() {
                            p.style_id = attr_value(&e, b"w:val");
                        }
                    } else if in_own_ppr && local == b"outlineLvl" {
                        if let Some(p) = current.as_mut() {
                            p.outline_lvl = attr_value(&e, b"w:val").and_then(|v| v.parse().ok());
                        }
                    } else if local == b"rPr" {
                        in_rpr = true;
                    } else if in_rpr && local == b"b" {
                        if let Some(p) = current.as_mut() {
                            p.is_bold = true;
                        }
                    } else if in_rpr && local == b"sz" {
                        if let Some(sz) =
                            attr_value(&e, b"w:val").and_then(|v| v.parse::<u32>().ok())
                        {
                            if let Some(p) = current.as_mut() {
                                p.max_sz = Some(p.max_sz.map_or(sz, |cur| cur.max(sz)));
                            }
                        }
                    }
                }
            }
            Ok(Event::Empty(e)) => {
                if fallback_depth.is_some() {
                    buf.clear();
                    continue;
                }
                let local = local_name(e.name().as_ref()).to_vec();
                if current.is_some() {
                    if local == b"tab" {
                        if let Some(p) = current.as_mut() {
                            p.text.push('\t');
                        }
                    } else if local == b"br" {
                        if let Some(p) = current.as_mut() {
                            p.text.push('\n');
                        }
                    } else if in_own_ppr && local == b"pStyle" {
                        if let Some(p) = current.as_mut() {
                            p.style_id = attr_value(&e, b"w:val");
                        }
                    } else if in_own_ppr && local == b"outlineLvl" {
                        if let Some(p) = current.as_mut() {
                            p.outline_lvl = attr_value(&e, b"w:val").and_then(|v| v.parse().ok());
                        }
                    } else if in_rpr && local == b"b" {
                        if let Some(p) = current.as_mut() {
                            p.is_bold = true;
                        }
                    } else if in_rpr && local == b"sz" {
                        if let Some(sz) =
                            attr_value(&e, b"w:val").and_then(|v| v.parse::<u32>().ok())
                        {
                            if let Some(p) = current.as_mut() {
                                p.max_sz = Some(p.max_sz.map_or(sz, |cur| cur.max(sz)));
                            }
                        }
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if fallback_depth.is_none() && in_t && ruby_depth == 0 {
                    if let Some(p) = current.as_mut() {
                        if let Ok(decoded) = t.decode() {
                            if let Ok(unescaped) = quick_xml::escape::unescape(&decoded) {
                                p.text.push_str(&unescaped);
                            }
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                let local = local_name(e.name().as_ref()).to_vec();
                if fallback_depth.is_some() {
                    if fallback_depth == Some(depth) {
                        fallback_depth = None;
                    }
                    depth = depth.saturating_sub(1);
                    buf.clear();
                    continue;
                }

                if local == b"p" {
                    paragraph_depth = paragraph_depth.saturating_sub(1);
                    if paragraph_depth == 0 {
                        if let Some(p) = current.take() {
                            results.push(p);
                        }
                    }
                } else if local == b"t" {
                    in_t = false;
                } else if local == b"rt" {
                    ruby_depth = ruby_depth.saturating_sub(1);
                } else if local == b"pPr" {
                    in_own_ppr = false;
                    if let Some(p) = current.as_mut() {
                        p.captured_own_ppr = true;
                    }
                } else if local == b"rPr" {
                    in_rpr = false;
                }
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006""#;

    fn doc(body: &str) -> String {
        format!(r#"<w:document {NS}><w:body>{body}</w:body></w:document>"#)
    }

    #[test]
    fn extracts_plain_paragraph_text() {
        let xml = doc(r#"<w:p><w:r><w:t>Hello world</w:t></w:r></w:p>"#);
        let paras = extract_paragraphs(xml.as_bytes(), &HashMap::new());
        assert_eq!(paras.len(), 1);
        assert_eq!(paras[0].text, "Hello world");
        assert_eq!(paras[0].heading_level, None);
    }

    #[test]
    fn skips_empty_paragraphs() {
        let xml = doc(r#"<w:p><w:r><w:t> </w:t></w:r></w:p><w:p><w:r><w:t>Real</w:t></w:r></w:p>"#);
        let paras = extract_paragraphs(xml.as_bytes(), &HashMap::new());
        assert_eq!(paras.len(), 1);
        assert_eq!(paras[0].text, "Real");
    }

    #[test]
    fn resolves_heading_via_style_id_to_name() {
        let mut styles = HashMap::new();
        styles.insert("Heading2".to_string(), "heading 2".to_string());
        let xml = doc(
            r#"<w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>A Heading</w:t></w:r></w:p>"#,
        );
        let paras = extract_paragraphs(xml.as_bytes(), &styles);
        assert_eq!(paras[0].heading_level, Some(2));
    }

    #[test]
    fn falls_back_to_outline_level_when_no_heading_style() {
        let xml =
            doc(r#"<w:p><w:pPr><w:outlineLvl w:val="1"/></w:pPr><w:r><w:t>Text</w:t></w:r></w:p>"#);
        let paras = extract_paragraphs(xml.as_bytes(), &HashMap::new());
        assert_eq!(paras[0].heading_level, Some(2));
    }

    #[test]
    fn falls_back_to_bold_and_size_heuristic() {
        let xml = doc(
            r#"<w:p><w:r><w:rPr><w:b/><w:sz w:val="28"/></w:rPr><w:t>Big Bold</w:t></w:r></w:p>"#,
        );
        let paras = extract_paragraphs(xml.as_bytes(), &HashMap::new());
        assert_eq!(paras[0].heading_level, Some(1));
    }

    #[test]
    fn reaches_paragraphs_inside_table_cells() {
        let xml = doc(
            r#"<w:tbl><w:tr><w:tc><w:p><w:r><w:t>Cell text</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
        );
        let paras = extract_paragraphs(xml.as_bytes(), &HashMap::new());
        assert_eq!(paras.len(), 1);
        assert_eq!(paras[0].text, "Cell text");
    }

    #[test]
    fn drops_mc_fallback_content_to_avoid_double_counting() {
        let xml = doc(r#"<w:p><w:r><mc:AlternateContent>
                <mc:Choice Requires="wps"><w:t>Choice text</w:t></mc:Choice>
                <mc:Fallback><w:t>Fallback text</w:t></mc:Fallback>
            </mc:AlternateContent></w:r></w:p>"#);
        let paras = extract_paragraphs(xml.as_bytes(), &HashMap::new());
        assert_eq!(paras.len(), 1);
        assert!(paras[0].text.contains("Choice text"));
        assert!(!paras[0].text.contains("Fallback text"));
    }

    #[test]
    fn non_integer_outline_lvl_does_not_panic() {
        let xml = doc(
            r#"<w:p><w:pPr><w:outlineLvl w:val="not-a-number"/></w:pPr><w:r><w:t>Text</w:t></w:r></w:p>"#,
        );
        let paras = extract_paragraphs(xml.as_bytes(), &HashMap::new());
        // Falls through to no heuristic matching -> no heading level, not a panic.
        assert_eq!(paras[0].heading_level, None);
    }

    #[test]
    fn ignores_field_codes_and_deleted_text_events() {
        // `w:instrText` (field codes) and `w:delText` (tracked-change
        // deletions) emit text events that a reader never sees — only the
        // real `w:t` content should survive extraction.
        let xml = doc(
            r#"<w:p><w:r><w:instrText> PAGE </w:instrText></w:r><w:r><w:t>Real</w:t></w:r><w:r><w:delText>gone</w:delText></w:r></w:p>"#,
        );
        let paras = extract_paragraphs(xml.as_bytes(), &HashMap::new());
        assert_eq!(paras.len(), 1);
        assert_eq!(paras[0].text, "Real");
    }

    #[test]
    fn ruby_gloss_is_not_pulled_into_paragraph_text() {
        // `<w:rt>` inside a ruby annotation should not leak its gloss into
        // the paragraph's text; only the base `<w:t>` is content.
        let xml = doc(
            r#"<w:p><w:r><w:ruby><w:rubyBase><w:r><w:t>base</w:t></w:r></w:rubyBase><w:rt><w:r><w:t>gloss</w:t></w:r></w:rt></w:ruby></w:r></w:p>"#,
        );
        let paras = extract_paragraphs(xml.as_bytes(), &HashMap::new());
        assert_eq!(paras.len(), 1);
        assert_eq!(paras[0].text, "base");
    }

    #[test]
    fn tab_and_break_inside_runs_are_materialized() {
        // `<w:tab/>` renders as a tab and `<w:br/>` as a line break —
        // matching python-docx's `paragraph.text`.
        let xml =
            doc(r#"<w:p><w:r><w:t>a</w:t><w:tab/><w:t>b</w:t><w:br/><w:t>c</w:t></w:r></w:p>"#);
        let paras = extract_paragraphs(xml.as_bytes(), &HashMap::new());
        assert_eq!(paras[0].text, "a\tb\nc");
    }

    #[test]
    fn inter_element_whitespace_is_not_paragraph_text() {
        // Pretty-printed XML puts newlines/spaces between runs; those are not
        // content and must not be folded into the extracted text.
        let xml = doc(r#"<w:p>
                <w:r><w:t>Hello</w:t></w:r>
                <w:r><w:t>World</w:t></w:r>
            </w:p>"#);
        let paras = extract_paragraphs(xml.as_bytes(), &HashMap::new());
        assert_eq!(paras.len(), 1);
        assert_eq!(paras[0].text, "HelloWorld");
    }
}
