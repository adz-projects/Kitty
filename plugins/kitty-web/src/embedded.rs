//! Reading the content a page shipped as data rather than markup.
//!
//! A page that "needs JavaScript" usually already has its content in the
//! HTML, just not where the DOM extractor looks: the framework's state blob
//! (`__NEXT_DATA__`, `__NUXT_DATA__`, `window.__INITIAL_STATE__ = {...}`),
//! a JSON-LD `articleBody`, or a `<noscript>` fallback. This module finds
//! that text without running any script. It is pure: `scrape.rs` calls it
//! when the DOM extraction comes back empty or thin.
//!
//! The JSON walk is deliberately generic rather than one parser per
//! framework: every string in every embedded JSON value is a candidate, and
//! a prose test keeps the ones that read like sentences. That catches state
//! shapes nobody wrote a rule for, at the cost of sometimes also picking up
//! a teaser or a consent notice — which is why it only runs when the DOM
//! gave little or nothing.

use std::collections::HashSet;
use std::sync::OnceLock;

use serde_json::Value;

use crate::scrape::{extract_markdown, markdown_to_text, strip_markdown_links};

/// Plain-text length under which a DOM extraction counts as thin, so the
/// embedded data gets a look.
pub const THIN_TEXT_CHARS: usize = 500;

/// The least plain text an embedded source must carry to be used at all.
const MIN_EMBEDDED_CHARS: usize = 200;

/// Blocks taken from one page's embedded JSON, at most.
const MAX_BLOCKS: usize = 300;

/// Content recovered from a page's embedded data.
#[derive(Debug, Clone, PartialEq)]
pub struct Embedded {
    pub markdown: String,
    /// Where it came from: `json-ld`, `noscript` or `script-data`.
    pub source: &'static str,
}

/// The best embedded content in `html`, or `None` when nothing carries
/// enough prose to be worth returning.
pub fn extract_embedded(html: &str, include_links: bool) -> Option<Embedded> {
    let document = scraper::Html::parse_document(html);
    let mut found: Vec<Embedded> = Vec::new();

    let (ld, scripts) = script_payloads(&document);
    if let Some(md) = json_ld_body(&ld, include_links) {
        found.push(Embedded {
            markdown: md,
            source: "json-ld",
        });
    }
    if let Some(md) = noscript_body(&document, include_links) {
        found.push(Embedded {
            markdown: md,
            source: "noscript",
        });
    }
    if let Some(md) = script_data_body(&scripts, include_links) {
        found.push(Embedded {
            markdown: md,
            source: "script-data",
        });
    }

    // JSON-LD is the publisher's own statement of the article body, so it
    // wins whenever it is substantial; otherwise the longest source does.
    found.retain(|e| plain_len(&e.markdown) >= MIN_EMBEDDED_CHARS);
    if let Some(pos) = found
        .iter()
        .position(|e| e.source == "json-ld" && plain_len(&e.markdown) >= 2 * MIN_EMBEDDED_CHARS)
    {
        return Some(found.swap_remove(pos));
    }
    found.into_iter().max_by_key(|e| plain_len(&e.markdown))
}

/// Plain-text length of a Markdown string, in chars.
pub fn plain_len(md: &str) -> usize {
    markdown_to_text(md).chars().count()
}

/// The parsed JSON in a page's scripts: JSON-LD values, and every other
/// value found (whole JSON scripts, and JSON assigned or `JSON.parse`d in
/// inline JavaScript), in document order.
fn script_payloads(document: &scraper::Html) -> (Vec<Value>, Vec<Value>) {
    let sel = scraper::Selector::parse("script").expect("static selector");
    let mut ld = Vec::new();
    let mut other = Vec::new();
    for script in document.select(&sel) {
        let kind = script
            .value()
            .attr("type")
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let text: String = script.text().collect();
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        if kind == "application/ld+json" {
            if let Ok(v) = serde_json::from_str::<Value>(text) {
                ld.push(v);
            }
        } else if kind.contains("json") {
            if let Ok(v) = serde_json::from_str::<Value>(strip_html_comment(text)) {
                other.push(v);
            }
        } else if kind.is_empty() || kind.contains("javascript") || kind == "module" {
            other.extend(json_in_javascript(text));
        }
    }
    (ld, other)
}

fn strip_html_comment(text: &str) -> &str {
    text.trim()
        .trim_start_matches("<!--")
        .trim_end_matches("-->")
        .trim()
}

/// JSON values embedded in inline JavaScript: the right-hand side of a
/// state assignment (`window.__INITIAL_STATE__ = {...}`) and the argument of
/// `JSON.parse("...")`. A value that is a JS object literal rather than
/// strict JSON (unquoted keys) is skipped, not guessed at.
fn json_in_javascript(js: &str) -> Vec<Value> {
    static ASSIGN: OnceLock<regex::Regex> = OnceLock::new();
    static PARSE: OnceLock<regex::Regex> = OnceLock::new();
    let assign = ASSIGN.get_or_init(|| {
        regex::Regex::new(
            r"(?:^|[\s;,(])(?:window\.|self\.|globalThis\.|var\s+|let\s+|const\s+)?[A-Za-z_$][\w$]*\s*=\s*([\[{])",
        )
        .expect("static regex")
    });
    let parse =
        PARSE.get_or_init(|| regex::Regex::new(r#"JSON\.parse\(\s*(")"#).expect("static regex"));

    let mut out = Vec::new();
    for caps in assign.captures_iter(js) {
        let start = caps.get(1).expect("group 1").start();
        if let Some(v) = first_json_value(&js[start..]) {
            if is_container(&v) {
                out.push(v);
            }
        }
    }
    for caps in parse.captures_iter(js) {
        let start = caps.get(1).expect("group 1").start();
        if let Some(Value::String(inner)) = first_json_value(&js[start..]) {
            if let Ok(v) = serde_json::from_str::<Value>(&inner) {
                out.push(v);
            }
        }
    }
    out
}

/// The first JSON value at the start of `s`, ignoring whatever follows it.
fn first_json_value(s: &str) -> Option<Value> {
    serde_json::Deserializer::from_str(s)
        .into_iter::<Value>()
        .next()?
        .ok()
}

fn is_container(v: &Value) -> bool {
    matches!(v, Value::Object(o) if !o.is_empty()) || matches!(v, Value::Array(a) if !a.is_empty())
}

/// `articleBody` (or a long `text`) from JSON-LD, with its headline.
fn json_ld_body(values: &[Value], include_links: bool) -> Option<String> {
    let mut blocks = Vec::new();
    let mut stack: Vec<&Value> = values.iter().collect();
    while let Some(v) = stack.pop() {
        match v {
            Value::Array(items) => stack.extend(items.iter().rev()),
            Value::Object(map) => {
                let body = ["articleBody", "text", "reviewBody"]
                    .iter()
                    .filter_map(|k| map.get(*k).and_then(Value::as_str))
                    .find(|s| s.chars().count() >= MIN_EMBEDDED_CHARS);
                if let Some(body) = body {
                    if let Some(title) = ["headline", "name"]
                        .iter()
                        .filter_map(|k| map.get(*k).and_then(Value::as_str))
                        .find(|t| is_title(t))
                    {
                        blocks.push(format!("## {}", title.trim()));
                    }
                    blocks.push(text_to_markdown(body, include_links));
                } else {
                    stack.extend(map.values().rev());
                }
            }
            _ => {}
        }
    }
    let md = blocks.join("\n\n").trim().to_string();
    (!md.is_empty()).then_some(md)
}

/// The page's `<noscript>` fallbacks, extracted like a page of their own.
///
/// `html5ever` parses with scripting on, so a `<noscript>`'s children are a
/// single raw-text node holding the fallback markup; that markup is
/// re-parsed here.
fn noscript_body(document: &scraper::Html, include_links: bool) -> Option<String> {
    let sel = scraper::Selector::parse("noscript").expect("static selector");
    let inner: Vec<String> = document
        .select(&sel)
        .map(|n| n.text().collect::<String>())
        .filter(|t| t.trim().len() > 40)
        .collect();
    if inner.is_empty() {
        return None;
    }
    let wrapped = format!("<html><body>{}</body></html>", inner.join("\n"));
    let (md, _) = extract_markdown(&wrapped, false, include_links)?;
    is_prose(&markdown_to_text(&md)).then_some(md)
}

/// Keys whose string values are never page prose.
const SKIP_KEYS: &[&str] = &[
    "css",
    "style",
    "styles",
    "classname",
    "class",
    "svg",
    "icon",
    "script",
    "scripts",
    "token",
    "hash",
    "query",
    "cookie",
    "cookies",
    "schema",
    "srcset",
    "path",
    "slug",
    "variables",
    "translations",
    "i18n",
    "messages",
    "locale",
    "locales",
];

fn skip_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    SKIP_KEYS.contains(&k.as_str())
        || k.ends_with("url")
        || k.ends_with("href")
        || k.ends_with("src")
        || k.ends_with("css")
        || k.ends_with("token")
}

/// Every prose-like string in the scripts' JSON, in order, each object's
/// title emitted ahead of its first prose field.
fn script_data_body(values: &[Value], include_links: bool) -> Option<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for v in values {
        walk(v, 0, include_links, &mut blocks, &mut seen);
        if blocks.len() >= MAX_BLOCKS {
            break;
        }
    }
    blocks.truncate(MAX_BLOCKS);
    // A run of headings with no prose under them is noise; drop trailing ones.
    while blocks.last().is_some_and(|b| b.starts_with("## ")) {
        blocks.pop();
    }
    let md = blocks.join("\n\n").trim().to_string();
    (!md.is_empty()).then_some(md)
}

/// serde_json refuses to parse past depth 128, so recursion is bounded by
/// the input; this cap is a second guard.
const MAX_WALK_DEPTH: usize = 160;

fn walk(
    v: &Value,
    depth: usize,
    include_links: bool,
    blocks: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    if depth > MAX_WALK_DEPTH || blocks.len() >= MAX_BLOCKS {
        return;
    }
    match v {
        Value::String(s) => push_prose(s, include_links, blocks, seen),
        Value::Array(items) => {
            for item in items {
                walk(item, depth + 1, include_links, blocks, seen);
            }
        }
        Value::Object(map) => {
            let mut titled = false;
            for (key, value) in map {
                if skip_key(key) {
                    continue;
                }
                if let Value::String(s) = value {
                    let before = blocks.len();
                    if !titled {
                        if let Some(title) = object_title(map) {
                            // Reserve the heading's slot; drop it again if
                            // this field turns out not to be prose.
                            blocks.push(format!("## {title}"));
                            push_prose(s, include_links, blocks, seen);
                            if blocks.len() == before + 1 {
                                blocks.pop();
                            } else {
                                titled = true;
                            }
                            continue;
                        }
                    }
                    push_prose(s, include_links, blocks, seen);
                } else {
                    walk(value, depth + 1, include_links, blocks, seen);
                }
            }
        }
        _ => {}
    }
}

fn object_title(map: &serde_json::Map<String, Value>) -> Option<String> {
    ["headline", "title", "heading", "name"]
        .iter()
        .filter_map(|k| map.get(*k).and_then(Value::as_str))
        .find(|t| is_title(t))
        .map(|t| t.trim().to_string())
}

fn is_title(t: &str) -> bool {
    let n = t.trim().chars().count();
    (3..=200).contains(&n) && !t.contains('<') && !t.starts_with("http")
}

fn push_prose(s: &str, include_links: bool, blocks: &mut Vec<String>, seen: &mut HashSet<String>) {
    if s.len() < 60 {
        return;
    }
    let md = text_to_markdown(s, include_links);
    let plain = markdown_to_text(&md);
    if !is_prose(&plain) {
        return;
    }
    let key: String = plain
        .chars()
        .filter(|c| c.is_alphanumeric())
        .take(120)
        .collect::<String>()
        .to_lowercase();
    if seen.insert(key) {
        blocks.push(md);
    }
}

/// A string as Markdown: an HTML fragment is converted, anything else is
/// taken as text.
fn text_to_markdown(s: &str, include_links: bool) -> String {
    static TAGS: OnceLock<regex::Regex> = OnceLock::new();
    let tags = TAGS.get_or_init(|| {
        regex::Regex::new(r"(?i)</?(p|div|h[1-6]|ul|ol|li|br|span|a|strong|em|b|i|table|tr|td|blockquote|section)\b")
            .expect("static regex")
    });
    if !tags.is_match(s) {
        return s.trim().to_string();
    }
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "noscript", "svg", "iframe"])
        .build();
    let md = converter
        .convert(s)
        .unwrap_or_else(|_| s.to_string())
        .trim()
        .to_string();
    if include_links {
        md
    } else {
        strip_markdown_links(&md)
    }
}

/// Whether a plain-text string reads like sentences rather than an id, a
/// blob, code or a UI label.
pub fn is_prose(text: &str) -> bool {
    let text = text.trim();
    if text.starts_with("http") || text.starts_with("data:") {
        return false;
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() < 12 {
        return false;
    }
    let non_ws: usize = words.iter().map(|w| w.chars().count()).sum();
    let avg = non_ws as f64 / words.len() as f64;
    if !(2.5..=12.0).contains(&avg) {
        return false;
    }
    let letters = text.chars().filter(|c| c.is_alphabetic()).count();
    if (letters as f64) < 0.6 * non_ws as f64 {
        return false;
    }
    let code_marks = text
        .chars()
        .filter(|c| matches!(c, '{' | '}' | ';' | '=' | '<' | '>'))
        .count();
    if code_marks * 20 > words.len() {
        return false;
    }
    text.contains(". ") || text.ends_with(['.', '!', '?', '"', '”'])
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARA: &str = "The committee published its annual review on Tuesday, finding that \
        regional rail ridership recovered to ninety percent of its earlier peak. Officials \
        said the remaining gap reflects permanent changes in commuting patterns.";

    #[test]
    fn prose_test_accepts_sentences_and_rejects_ids_blobs_and_code() {
        assert!(is_prose(PARA));
        assert!(!is_prose("Read more"));
        assert!(!is_prose(&"aGVsbG8gd29ybGQ".repeat(20)));
        assert!(!is_prose(
            "function a(){return b;} var c = {d: 1}; if (e) { f = g; } else { h = i; } j = k;"
        ));
        assert!(!is_prose(
            "https://example.com/a/very/long/path/that/goes/on and on and on with words in it"
        ));
    }

    #[test]
    fn next_data_state_yields_the_article() {
        let html = format!(
            r#"<html><body><div id="__next"></div>
            <script id="__NEXT_DATA__" type="application/json">
            {{"props":{{"pageProps":{{"post":{{"title":"Ridership recovers","imageUrl":"https://x/y.png",
            "body":"<p>{PARA}</p><p>{PARA} Again.</p>"}}}}}}}}
            </script></body></html>"#
        );
        let e = extract_embedded(&html, false).expect("found");
        assert_eq!(e.source, "script-data");
        assert!(
            e.markdown.starts_with("## Ridership recovers"),
            "{}",
            e.markdown
        );
        assert!(e.markdown.contains("regional rail ridership"));
        assert!(!e.markdown.contains("x/y.png"));
    }

    #[test]
    fn window_state_assignment_and_json_parse_are_read() {
        let state = serde_json::json!({"article": {"text": PARA}}).to_string();
        let html = format!(
            "<html><body><script>window.__INITIAL_STATE__ = {state};\nfoo();</script></body></html>"
        );
        let e = extract_embedded(&html, false).expect("assignment");
        assert!(e.markdown.contains("annual review"));

        let quoted = serde_json::to_string(&state).unwrap();
        let html =
            format!("<html><body><script>var s = JSON.parse({quoted});</script></body></html>");
        let e = extract_embedded(&html, false).expect("JSON.parse");
        assert!(e.markdown.contains("annual review"));
    }

    #[test]
    fn json_ld_article_body_wins_when_substantial() {
        let body = format!("{PARA} {PARA} {PARA}");
        let ld = serde_json::json!({"@context":"https://schema.org","@graph":[
            {"@type":"WebSite","name":"Site"},
            {"@type":"NewsArticle","headline":"Rail review","articleBody": body}
        ]});
        let html = format!(
            r#"<html><head><script type="application/ld+json">{ld}</script></head>
            <body><script>window.__S__ = {{"teaser":"{PARA}"}};</script></body></html>"#
        );
        let e = extract_embedded(&html, false).expect("found");
        assert_eq!(e.source, "json-ld");
        assert!(e.markdown.starts_with("## Rail review"));
    }

    #[test]
    fn noscript_fallback_is_extracted() {
        let html = format!(
            "<html><body><div id=app></div><noscript><article><h1>Review</h1><p>{PARA}</p><p>{PARA}</p></article></noscript></body></html>"
        );
        let e = extract_embedded(&html, false).expect("found");
        assert_eq!(e.source, "noscript");
        assert!(e.markdown.contains("annual review"));
    }

    #[test]
    fn a_bare_enable_javascript_notice_is_not_content() {
        let html = "<html><body><noscript>You need to enable JavaScript to run this app.</noscript>\
            <script>window.cfg = {\"apiBase\":\"https://api.x\",\"label\":\"Sign in\"};</script></body></html>";
        assert!(extract_embedded(html, false).is_none());
    }

    #[test]
    fn duplicated_strings_appear_once() {
        let state = serde_json::json!({"a": {"text": PARA}, "b": {"text": PARA}}).to_string();
        let html = format!("<html><body><script>window.s = {state};</script></body></html>");
        let e = extract_embedded(&html, false).expect("found");
        assert_eq!(e.markdown.matches("annual review").count(), 1);
    }
}
