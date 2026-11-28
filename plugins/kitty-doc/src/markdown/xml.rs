//! A small owned XML tree for the document-to-markdown converters.
//!
//! The paragraph scanner in `docx::read` streams events because it only needs
//! flat text. Turning a document into structured markdown needs to look at a
//! paragraph's properties before its runs, a table's rows as a whole and a
//! list's nesting, which is far simpler over a tree. Parts are capped at
//! `MAX_DOCX_ENTRY_BYTES` before they get here, so holding one in memory is
//! bounded.
//!
//! Entity references are resolved: quick-xml (≥ 0.38) reports `&amp;`,
//! `&lt;` and character references as their own events, separate from the
//! surrounding text, and a reader that ignores them silently drops every `&`
//! and `<` in a document.

use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::reader::Reader;

#[derive(Debug, Clone, Default)]
pub struct Element {
    /// The qualified name as written (`w:p`, `text:span`).
    pub name: String,
    /// Attributes by qualified key.
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

#[derive(Debug, Clone)]
pub enum Node {
    Element(Element),
    Text(String),
}

fn local(qualified: &str) -> &str {
    qualified.rsplit(':').next().unwrap_or(qualified)
}

impl Element {
    /// The name without its namespace prefix (`p` for `w:p`).
    pub fn local(&self) -> &str {
        local(&self.name)
    }

    pub fn is(&self, local_name: &str) -> bool {
        self.local() == local_name
    }

    /// An attribute by its local name (`val` finds `w:val`).
    pub fn attr(&self, local_name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| local(k) == local_name)
            .map(|(_, v)| v.as_str())
    }

    /// An attribute by its exact qualified key (`r:id`, not `w:id`).
    pub fn attr_q(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|c| match c {
            Node::Element(e) => Some(e),
            Node::Text(_) => None,
        })
    }

    /// The first direct child with this local name.
    pub fn child(&self, local_name: &str) -> Option<&Element> {
        self.elements().find(|e| e.is(local_name))
    }

    /// Follow a path of local names through first matches.
    pub fn path(&self, names: &[&str]) -> Option<&Element> {
        let mut at = self;
        for n in names {
            at = at.child(n)?;
        }
        Some(at)
    }

    /// The first descendant (depth first) with this local name.
    pub fn find(&self, local_name: &str) -> Option<&Element> {
        for e in self.elements() {
            if e.is(local_name) {
                return Some(e);
            }
            if let Some(found) = e.find(local_name) {
                return Some(found);
            }
        }
        None
    }

    /// Every descendant with this local name, in document order.
    pub fn find_all<'a>(&'a self, local_name: &str, out: &mut Vec<&'a Element>) {
        for e in self.elements() {
            if e.is(local_name) {
                out.push(e);
            }
            e.find_all(local_name, out);
        }
    }

    /// All text beneath this element, concatenated.
    pub fn text(&self) -> String {
        let mut out = String::new();
        self.collect_text(&mut out);
        out
    }

    fn collect_text(&self, out: &mut String) {
        for c in &self.children {
            match c {
                Node::Text(t) => out.push_str(t),
                Node::Element(e) => e.collect_text(out),
            }
        }
    }
}

fn start_element(e: &BytesStart) -> Element {
    let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    let attrs = e
        .attributes()
        .flatten()
        .map(|a| {
            let key = String::from_utf8_lossy(a.key.as_ref()).into_owned();
            let value = a
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map(|v| v.into_owned())
                .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned());
            (key, value)
        })
        .collect();
    Element {
        name,
        attrs,
        children: Vec::new(),
    }
}

/// The text an entity or character reference stands for.
pub(crate) fn resolve_ref(r: &BytesRef) -> Option<String> {
    if r.is_char_ref() {
        return r.resolve_char_ref().ok().flatten().map(String::from);
    }
    let name = r.decode().ok()?;
    let s = match name.as_ref() {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        _ => return None,
    };
    Some(s.to_string())
}

fn push_text(stack: &mut [Element], text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(top) = stack.last_mut() {
        if let Some(Node::Text(prev)) = top.children.last_mut() {
            prev.push_str(text);
        } else {
            top.children.push(Node::Text(text.to_string()));
        }
    }
}

/// Parse a whole XML part into a tree rooted at a synthetic element whose
/// children are the document's top-level nodes. Malformed XML yields what was
/// read before the error.
pub fn parse(xml: &[u8]) -> Element {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut stack: Vec<Element> = vec![Element::default()];
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => stack.push(start_element(&e)),
            Ok(Event::Empty(e)) => {
                let el = start_element(&e);
                if let Some(top) = stack.last_mut() {
                    top.children.push(Node::Element(el));
                }
            }
            Ok(Event::End(_)) => {
                if stack.len() > 1 {
                    let done = stack.pop().unwrap_or_default();
                    if let Some(top) = stack.last_mut() {
                        top.children.push(Node::Element(done));
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if let Ok(decoded) = t.decode() {
                    push_text(&mut stack, &decoded);
                }
            }
            Ok(Event::CData(t)) => {
                push_text(&mut stack, &String::from_utf8_lossy(&t));
            }
            Ok(Event::GeneralRef(r)) => {
                if let Some(s) = resolve_ref(&r) {
                    push_text(&mut stack, &s);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    // Close anything left open by a truncated document.
    while stack.len() > 1 {
        let done = stack.pop().unwrap_or_default();
        if let Some(top) = stack.last_mut() {
            top.children.push(Node::Element(done));
        }
    }
    stack.pop().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_references_are_kept_as_text() {
        let root = parse(br#"<w:t>Tom &amp; Jerry &lt;3 &#8212; &#x41;</w:t>"#);
        assert_eq!(root.text(), "Tom & Jerry <3 \u{2014} A");
    }

    #[test]
    fn attributes_resolve_by_local_or_qualified_name() {
        let root = parse(br#"<a><w:hyperlink r:id="rId4" w:anchor="x"/></a>"#);
        let link = root.find("hyperlink").unwrap();
        assert_eq!(link.attr("id"), Some("rId4"));
        assert_eq!(link.attr_q("r:id"), Some("rId4"));
        assert_eq!(link.attr("anchor"), Some("x"));
    }
}
