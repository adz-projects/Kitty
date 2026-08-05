//! `lean_web_scrape` — Rust port of `kitty_docs_web.py`'s page scraper.
//!
//! The extraction stack is the one unavoidable substitution: `trafilatura`
//! has no Rust equivalent, so body extraction is `scraper`-based
//! boilerplate-stripping into a main-content subtree, then `htmd` to render
//! that subtree as Markdown. Everything downstream of extraction — block
//! splitting, link stripping, char capping, query filtering, the metadata
//! map and every error code/hint — is a faithful port, because that's what
//! the model's prompt-visible contract actually is.

use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::envelope::{error_response, success_response};
use crate::query_filter::filter_by_query;

pub const SCRAPE_MAX_CHARS_DEFAULT: usize = 12000;

/// A complete, current-looking UA string. The Python original's note applies
/// verbatim: a truncated UA ending at `AppleWebKit/537.36` with no
/// `(KHTML, like Gecko) Chrome/... Safari/...` tail is a shape several WAFs
/// fingerprint directly as non-browser traffic, and an avoidable cause of
/// 403s. Also reused by `search::ddg_query`.
pub const SCRAPE_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// Splits markdown on blank-line boundaries, keeping a fenced code block
/// intact even if it contains a blank line (a naive `\n\n` split would
/// otherwise slice through an open ``` fence and reassemble it as two
/// separate, malformed "paragraphs").
pub fn split_markdown_blocks(text: &str) -> Vec<String> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\n{3,}").expect("static regex is valid"));

    let normalized = re.replace_all(text.trim(), "\n\n").into_owned();
    let mut blocks: Vec<String> = Vec::new();
    let mut buffer: Vec<String> = Vec::new();
    let mut fence_open = false;

    for block in normalized.split("\n\n") {
        buffer.push(block.to_string());
        if block.matches("```").count() % 2 == 1 {
            fence_open = !fence_open;
        }
        if !fence_open {
            blocks.push(buffer.join("\n\n"));
            buffer.clear();
        }
    }
    if !buffer.is_empty() {
        blocks.push(buffer.join("\n\n"));
    }

    blocks
        .into_iter()
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
        .collect()
}

/// `[label](url)` -> `label`, but leaves fenced code blocks untouched so a
/// URL inside a code sample is never rewritten.
pub fn strip_markdown_links(text: &str) -> String {
    use std::sync::OnceLock;
    static FENCE: OnceLock<regex::Regex> = OnceLock::new();
    static LINK: OnceLock<regex::Regex> = OnceLock::new();
    let fence = FENCE.get_or_init(|| regex::Regex::new(r"(?s)```.*?```").expect("static regex is valid"));
    let link = LINK.get_or_init(|| regex::Regex::new(r"\[([^\]]+)\]\([^\)]+\)").expect("static regex is valid"));

    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    for m in fence.find_iter(text) {
        out.push_str(&link.replace_all(&text[last..m.start()], "$1"));
        out.push_str(m.as_str());
        last = m.end();
    }
    out.push_str(&link.replace_all(&text[last..], "$1"));
    out
}

/// Greedily takes whole blocks up to `cap` characters, never cutting
/// mid-block (mid-table, mid-heading) except when a single block alone
/// already exceeds the cap, in which case that one block is truncated so the
/// tool still returns *something* rather than nothing.
pub fn cap_blocks_by_chars(blocks: &[String], cap: usize) -> (String, usize) {
    if blocks.is_empty() {
        return (String::new(), 0);
    }
    if blocks[0].chars().count() > cap {
        return (blocks[0].chars().take(cap).collect(), 1);
    }
    let mut selected: Vec<&String> = Vec::new();
    let mut running = 0usize;
    for b in blocks {
        let block_len = b.chars().count() + if selected.is_empty() { 0 } else { 2 };
        if !selected.is_empty() && running + block_len > cap {
            break;
        }
        selected.push(b);
        running += block_len;
    }
    let n = selected.len();
    (
        selected
            .into_iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
        n,
    )
}

/// Best-effort Markdown -> plain-text renderer backing `output_format="text"`.
///
/// The Python original handed this choice to `trafilatura.extract(...,
/// output_format=...)`, which has no Rust equivalent (the same reason `htmd`
/// stands in for it here), so this is the domestic twin of that substitution:
/// it strips the common Markdown syntax `htmd` emits — headings, blockquotes,
/// emphasis, inline/code fences, lists, links, tables, thematic breaks — down
/// to readable prose. Deterministic and intentionally tolerant: an
/// unhandled construct is left as-is rather than erroring.
pub fn markdown_to_text(md: &str) -> String {
    use std::sync::OnceLock;
    static LINK: OnceLock<regex::Regex> = OnceLock::new();
    static EMPH: OnceLock<regex::Regex> = OnceLock::new();
    let link = LINK.get_or_init(|| regex::Regex::new(r"\[([^\]]+)\]\([^\)]+\)").expect("static regex is valid"));
    // Emphasis markers (`**`/`__`/inline-code backticks/single `*`/`_`) are
    // pure formatting the text mode wants gone entirely.
    let emph = EMPH.get_or_init(|| regex::Regex::new(r"[*_`]").expect("static regex is valid"));

    let mut out = String::new();
    for line in md.lines() {
        let t = line.trim();
        if t.is_empty() {
            out.push('\n');
            continue;
        }
        // Fences and thematic/table separators carry no prose.
        if t.starts_with("```") || t == "---" || t == "===" || t.contains("|---") {
            out.push('\n');
            continue;
        }
        // Headings / blockquotes: strip the leading marker(s).
        let mut content: &str = t;
        while content.starts_with('#') || content.starts_with('>') {
            content = &content[1..];
        }
        let content = content.trim_start();
        // List items and ordered "N." markers become plain "- " lines.
        let content: &str = content
            .strip_prefix("- ")
            .or_else(|| content.strip_prefix("* "))
            .or_else(|| content.strip_prefix("+ "))
            .or_else(|| strip_ordered_marker(content))
            .unwrap_or(content);
        // A table row "| a | b |" -> "a   b".
        let rendered = if content.starts_with('|') {
            content
                .trim_matches('|')
                .split('|')
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .collect::<Vec<_>>()
                .join("   ")
        } else {
            content.to_string()
        };
        // Inline: links -> labels, then strip emphasis markers.
        let no_links = link.replace_all(&rendered, "$1").into_owned();
        let plain = emph.replace_all(&no_links, "").into_owned();
        if !plain.trim().is_empty() {
            out.push_str(plain.trim());
            out.push('\n');
        }
    }
    out
}

fn strip_ordered_marker(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    bytes
        .get(i)
        .filter(|b| **b == b'.')
        .map(|_| s[i + 1..].trim_start())
}

/// Metadata a scrape reports alongside the body.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PageMeta {
    pub title: Option<String>,
    pub sitename: Option<String>,
    pub date: Option<String>,
}

/// Tags that never contain article prose. Removed before extraction so
/// navigation, cookie banners and script bodies can't end up in the Markdown
/// — this is the boilerplate-stripping half of what `trafilatura` does.
const BOILERPLATE_TAGS: [&str; 12] = [
    "script", "style", "noscript", "nav", "header", "footer", "aside", "form", "svg", "iframe",
    "template", "button",
];

/// Content roots tried in order — the first one present wins, mirroring
/// `trafilatura`'s preference for a semantic main-content container over the
/// whole `<body>`. Falling through to `body` is the recall-favoring default
/// the Python docstring describes.
const CONTENT_SELECTORS: [&str; 6] = ["article", "main", "[role=main]", "#content", ".content", "body"];

/// Extracts `(markdown_body, metadata)` from an HTML document.
///
/// Returns `None` when nothing usable is found, which the caller turns into
/// `SCRAPE_EMPTY` — the same contract `trafilatura.extract` returning `None`
/// had.
///
/// `favor_precision` narrows the accepted content roots (dropping the
/// `body`-wide fallback), matching the Python parameter's documented intent:
/// the default favors *recall*, because documentation/API-reference pages
/// often lose sidebars and short definition blocks under precision mode.
pub fn extract_markdown(html: &str, favor_precision: bool, include_links: bool) -> Option<(String, PageMeta)> {
    use scraper::{Html, Selector};

    let document = Html::parse_document(html);
    let meta = extract_metadata(&document);

    // `scraper` gives an immutable DOM, so boilerplate is removed by
    // collecting the ids of unwanted subtrees and skipping them during
    // serialization rather than by mutating the tree.
    let mut excluded: std::collections::HashSet<ego_tree::NodeId> = std::collections::HashSet::new();
    for tag in BOILERPLATE_TAGS {
        if let Ok(sel) = Selector::parse(tag) {
            for el in document.select(&sel) {
                collect_subtree(el, &mut excluded);
            }
        }
    }

    let roots: &[&str] = if favor_precision {
        &CONTENT_SELECTORS[..CONTENT_SELECTORS.len() - 1]
    } else {
        &CONTENT_SELECTORS
    };

    for candidate in roots {
        let Ok(sel) = Selector::parse(candidate) else {
            continue;
        };
        let Some(root) = document.select(&sel).next() else {
            continue;
        };
        if excluded.contains(&root.id()) {
            continue;
        }
        let cleaned_html = serialize_without(root, &excluded);
        let converter = htmd::HtmlToMarkdown::builder()
            .skip_tags(vec!["script", "style", "noscript"])
            .build();
        let Ok(md) = converter.convert(&cleaned_html) else {
            continue;
        };
        let md = md.trim();
        if !md.is_empty() {
            let md = if include_links {
                md.to_string()
            } else {
                strip_markdown_links(md)
            };
            return Some((md, meta));
        }
    }
    None
}

fn collect_subtree(el: scraper::ElementRef, out: &mut std::collections::HashSet<ego_tree::NodeId>) {
    out.insert(el.id());
    for descendant in el.descendants() {
        out.insert(descendant.id());
    }
}

/// Re-serializes an element's subtree, skipping excluded nodes. Attributes
/// are preserved for the tags `htmd` needs them on (`a[href]`, `img[alt]`),
/// which is why this can't just be a text dump.
fn serialize_without(
    root: scraper::ElementRef,
    excluded: &std::collections::HashSet<ego_tree::NodeId>,
) -> String {
    let mut out = String::new();
    serialize_node(*root, excluded, &mut out);
    out
}

fn serialize_node(
    node: ego_tree::NodeRef<scraper::Node>,
    excluded: &std::collections::HashSet<ego_tree::NodeId>,
    out: &mut String,
) {
    if excluded.contains(&node.id()) {
        return;
    }
    match node.value() {
        scraper::Node::Text(t) => {
            // Escaping matters: raw `<` in text would otherwise re-parse as
            // markup on htmd's side.
            for ch in t.chars() {
                match ch {
                    '<' => out.push_str("&lt;"),
                    '>' => out.push_str("&gt;"),
                    '&' => out.push_str("&amp;"),
                    c => out.push(c),
                }
            }
        }
        scraper::Node::Element(el) => {
            let name = el.name();
            out.push('<');
            out.push_str(name);
            for (attr, value) in el.attrs() {
                if matches!(attr, "href" | "src" | "alt" | "title") {
                    out.push(' ');
                    out.push_str(attr);
                    out.push_str("=\"");
                    for ch in value.chars() {
                        match ch {
                            '"' => out.push_str("&quot;"),
                            '&' => out.push_str("&amp;"),
                            c => out.push(c),
                        }
                    }
                    out.push('"');
                }
            }
            out.push('>');
            for child in node.children() {
                serialize_node(child, excluded, out);
            }
            out.push_str("</");
            out.push_str(name);
            out.push('>');
        }
        _ => {
            for child in node.children() {
                serialize_node(child, excluded, out);
            }
        }
    }
}

fn extract_metadata(document: &scraper::Html) -> PageMeta {
    use scraper::Selector;

    let meta_content = |selector: &str| -> Option<String> {
        let sel = Selector::parse(selector).ok()?;
        let el = document.select(&sel).next()?;
        el.value()
            .attr("content")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let title = meta_content(r#"meta[property="og:title"]"#).or_else(|| {
        let sel = Selector::parse("title").ok()?;
        let el = document.select(&sel).next()?;
        let text = el.text().collect::<String>().trim().to_string();
        (!text.is_empty()).then_some(text)
    });

    let sitename = meta_content(r#"meta[property="og:site_name"]"#);
    let date = meta_content(r#"meta[property="article:published_time"]"#)
        .or_else(|| meta_content(r#"meta[name="date"]"#))
        .or_else(|| meta_content(r#"meta[itemprop="datePublished"]"#));

    PageMeta { title, sitename, date }
}

/// Filename sanitization for a downloaded PDF, matching Python's
/// `re.sub(r"[^\w.\-]", "_", ...)`.
fn pdf_filename_for(stripped_url: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"[^\w.\-]").expect("static regex is valid"));

    let tail = stripped_url.rsplit('/').next().unwrap_or("");
    let tail = if tail.is_empty() { "downloaded.pdf" } else { tail };
    let mut name = re.replace_all(tail, "_").into_owned();
    if !name.to_lowercase().ends_with(".pdf") {
        name.push_str(".pdf");
    }
    name
}

/// Same cache directory the Python tools use, so a PDF downloaded here stays
/// readable by `lean_pdf_read_text` (still hosted by `kitty-docs-web`) and
/// inspectable via `kitty-tools`' `lean_cache_view` without the two halves
/// having to agree on a second location.
fn cache_dir() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".cache")
        .join("lean-goose-mcp")
}

#[allow(clippy::too_many_arguments)]
pub async fn web_scrape(
    url: &str,
    query: Option<&str>,
    output_format: &str,
    offset: usize,
    max_chars: Option<usize>,
    include_links: bool,
    favor_precision: bool,
) -> String {
    let stripped_url = url.split('?').next().unwrap_or(url);
    let stripped_url = stripped_url.split('#').next().unwrap_or(stripped_url);
    let looks_like_pdf = stripped_url.to_lowercase().ends_with(".pdf");

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return error_response(
                "SCRAPE_NETWORK_ERROR",
                "Failed to build the HTTP client.",
                Some(&e.to_string()),
                Some("Check host connectivity, or try a different URL."),
            );
        }
    };

    let response = match client
        .get(url)
        .header("User-Agent", SCRAPE_USER_AGENT)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,application/pdf;q=0.8,*/*;q=0.5",
        )
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            if e.is_timeout() {
                return error_response(
                    "SCRAPE_TIMEOUT",
                    "Request timed out.",
                    Some(&format!("{url}: {e}")),
                    Some("The server is slow or unreachable. Try again or use lean_web_search for a similar page."),
                );
            }
            return error_response(
                "SCRAPE_NETWORK_ERROR",
                "Failed to communicate with the server.",
                Some(&format!("{url}: {e}")),
                Some("Check host connectivity, or try a different URL."),
            );
        }
    };

    let status = response.status();
    if !status.is_success() {
        return error_response(
            "SCRAPE_HTTP_ERROR",
            &format!("HTTP {} fetching URL.", status.as_u16()),
            Some(&format!("{url}: HTTP {status}")),
            Some(
                "The page may be behind a login wall, blocked, or deleted. Try \
                 lean_web_search for an alternative source.",
            ),
        );
    }

    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();

    let is_pdf = looks_like_pdf || content_type == "application/pdf";

    if is_pdf {
        let bytes = match response.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return error_response(
                    "SCRAPE_NETWORK_ERROR",
                    "Failed to download the PDF body.",
                    Some(&format!("{url}: {e}")),
                    Some("Check host connectivity, or try a different URL."),
                );
            }
        };
        let dir = cache_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return error_response(
                "SCRAPE_NETWORK_ERROR",
                "Could not create the cache directory for the downloaded PDF.",
                Some(&e.to_string()),
                Some("Check filesystem permissions for the user cache directory."),
            );
        }
        let pdf_path = dir.join(pdf_filename_for(stripped_url));
        if let Err(e) = std::fs::write(&pdf_path, &bytes) {
            return error_response(
                "SCRAPE_NETWORK_ERROR",
                "Could not write the downloaded PDF to cache.",
                Some(&e.to_string()),
                Some("Check filesystem permissions for the user cache directory."),
            );
        }
        return success_response(
            json!({"cached_path": pdf_path.to_string_lossy(), "url": url}),
            Some(
                "URL is a PDF; downloaded to cache. Use lean_pdf_read_text or \
                 lean_pdf_read_outline on the cached_path above.",
            ),
            false,
            None,
        );
    }

    if !(content_type.starts_with("text/html") || content_type.starts_with("application/xhtml")) {
        let shown = if content_type.is_empty() { "unknown" } else { &content_type };
        return error_response(
            "SCRAPE_UNSUPPORTED_CONTENT_TYPE",
            &format!("URL did not return an HTML page (Content-Type: {shown})."),
            Some(url),
            Some("This tool extracts article/documentation body text from HTML pages only."),
        );
    }

    let html = match response.text().await {
        Ok(t) => t,
        Err(e) => {
            return error_response(
                "SCRAPE_NETWORK_ERROR",
                "Failed to read the response body.",
                Some(&format!("{url}: {e}")),
                Some("Check host connectivity, or try a different URL."),
            );
        }
    };

    render_scrape_result(
        &html,
        url,
        &final_url,
        &content_type,
        query,
        output_format,
        offset,
        max_chars,
        include_links,
        favor_precision,
    )
}

/// The pure half of `web_scrape`: everything after the HTTP response body is
/// in hand. Split out so the whole capping/filtering/metadata contract is
/// testable without a network round trip.
#[allow(clippy::too_many_arguments)]
pub fn render_scrape_result(
    html: &str,
    url: &str,
    final_url: &str,
    content_type: &str,
    query: Option<&str>,
    output_format: &str,
    offset: usize,
    max_chars: Option<usize>,
    include_links: bool,
    favor_precision: bool,
) -> String {
    let text_mode = output_format.eq_ignore_ascii_case("text");

    let Some((body_md, page_meta)) = extract_markdown(html, favor_precision, include_links) else {
        return error_response(
            "SCRAPE_EMPTY",
            "No extractable body content found.",
            Some(url),
            Some(
                "The page may be a JavaScript SPA or behind a paywall. Try a different URL \
                 or use lean_web_search.",
            ),
        );
    };

    if body_md.trim().is_empty() {
        return error_response(
            "SCRAPE_EMPTY",
            "No extractable body content found.",
            Some(url),
            Some(
                "The page may be a JavaScript SPA or behind a paywall. Try a different URL \
                 or use lean_web_search.",
            ),
        );
    }

    let blocks = split_markdown_blocks(&body_md);
    let cap = max_chars.unwrap_or(SCRAPE_MAX_CHARS_DEFAULT);

    let mut base_meta = Map::new();
    base_meta.insert("url".into(), json!(url));
    base_meta.insert("final_url".into(), json!(final_url));
    base_meta.insert("title".into(), json!(page_meta.title));
    base_meta.insert("sitename".into(), json!(page_meta.sitename));
    base_meta.insert("date".into(), json!(page_meta.date));
    base_meta.insert("content_type".into(), json!(content_type));

    if let Some(q) = query.filter(|q| !q.trim().is_empty()) {
        let result = filter_by_query(&blocks, Some(q), 50, offset);
        let (mut capped_text, n_used) = cap_blocks_by_chars(&result.items, cap);
        if text_mode {
            capped_text = markdown_to_text(&capped_text);
        }
        let char_truncated = n_used < result.items.len();
        let message = result
            .no_match
            .then(|| format!("No direct matches for query '{q}'. Showing top section."));

        let mut meta = base_meta;
        meta.insert("filtered_by_query".into(), json!(q));
        meta.insert("total_matches".into(), json!(result.total_matches));
        meta.insert("offset".into(), json!(offset));
        meta.insert("char_count_returned".into(), json!(capped_text.chars().count()));
        if let Some(next) = result.next_offset {
            meta.insert("next_offset".into(), json!(next));
        } else if char_truncated {
            meta.insert("next_offset".into(), json!(offset + n_used));
        }

        return success_response(
            json!(capped_text),
            message.as_deref(),
            result.truncated || char_truncated,
            Some(Value::Object(meta)),
        );
    }

    let remaining: Vec<String> = blocks.iter().skip(offset).cloned().collect();
    let (mut returned_text, n_used) = cap_blocks_by_chars(&remaining, cap);
    if text_mode {
        returned_text = markdown_to_text(&returned_text);
    }
    let end_idx = offset + n_used;
    let has_more = end_idx < blocks.len();
    let full_len: usize = blocks.join("\n\n").chars().count();

    let mut meta = base_meta;
    meta.insert("char_count_returned".into(), json!(returned_text.chars().count()));
    meta.insert("char_count_total".into(), json!(full_len));
    meta.insert("offset".into(), json!(offset));
    if has_more {
        meta.insert("next_offset".into(), json!(end_idx));
    }

    success_response(json!(returned_text), None, has_more, Some(Value::Object(meta)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn split_blocks_keeps_fenced_code_with_blank_lines_intact() {
        let md = "Intro paragraph.\n\n```rust\nfn a() {}\n\nfn b() {}\n```\n\nOutro.";
        let blocks = split_markdown_blocks(md);
        assert_eq!(blocks.len(), 3, "got {blocks:#?}");
        assert!(blocks[1].starts_with("```rust"));
        assert!(blocks[1].ends_with("```"));
        assert!(blocks[1].contains("fn a()") && blocks[1].contains("fn b()"));
    }

    #[test]
    fn split_blocks_collapses_runs_of_blank_lines() {
        assert_eq!(split_markdown_blocks("a\n\n\n\n\nb"), s(&["a", "b"]));
    }

    #[test]
    fn split_blocks_on_empty_input_is_empty() {
        assert!(split_markdown_blocks("").is_empty());
        assert!(split_markdown_blocks("   \n\n  ").is_empty());
    }

    #[test]
    fn strip_links_rewrites_prose_but_not_code_fences() {
        let md = "See [the docs](https://example.com/docs).\n\n```\ncurl [x](https://y.z)\n```\n\nAnd [more](http://m.n).";
        let out = strip_markdown_links(md);
        assert!(out.contains("See the docs."));
        assert!(out.contains("And more."));
        // The URL inside the fence must survive untouched.
        assert!(out.contains("curl [x](https://y.z)"), "got {out}");
    }

    #[test]
    fn cap_blocks_never_cuts_mid_block() {
        let blocks = s(&["aaaa", "bbbb", "cccc"]);
        let (text, n) = cap_blocks_by_chars(&blocks, 10);
        // "aaaa" + "\n\n" + "bbbb" = 10 chars exactly; a third would exceed.
        assert_eq!(n, 2);
        assert_eq!(text, "aaaa\n\nbbbb");
    }

    #[test]
    fn cap_blocks_truncates_a_single_oversized_block_rather_than_returning_nothing() {
        let blocks = s(&["x".repeat(100).as_str()]);
        let (text, n) = cap_blocks_by_chars(&blocks, 10);
        assert_eq!(n, 1);
        assert_eq!(text.chars().count(), 10);
    }

    #[test]
    fn cap_blocks_on_empty_input() {
        let (text, n) = cap_blocks_by_chars(&[], 100);
        assert!(text.is_empty());
        assert_eq!(n, 0);
    }

    const PAGE: &str = r#"
    <html>
      <head>
        <title>Fallback Title</title>
        <meta property="og:title" content="Real Title">
        <meta property="og:site_name" content="Example Site">
        <meta property="article:published_time" content="2025-03-04">
      </head>
      <body>
        <nav><a href="/somewhere">Navigation link</a></nav>
        <script>var tracking = "SHOULD_NOT_APPEAR";</script>
        <article>
          <h1>Main Heading</h1>
          <p>First paragraph with <a href="https://example.com">a link</a>.</p>
          <p>Second paragraph.</p>
        </article>
        <footer>Footer boilerplate</footer>
      </body>
    </html>
    "#;

    #[test]
    fn extract_prefers_article_and_drops_boilerplate() {
        let (md, meta) = extract_markdown(PAGE, false, false).expect("should extract");
        assert!(md.contains("Main Heading"));
        assert!(md.contains("First paragraph"));
        assert!(md.contains("Second paragraph"));
        assert!(!md.contains("SHOULD_NOT_APPEAR"), "script body leaked: {md}");
        assert!(!md.contains("Navigation link"), "nav leaked: {md}");
        assert!(!md.contains("Footer boilerplate"), "footer leaked: {md}");

        assert_eq!(meta.title.as_deref(), Some("Real Title"));
        assert_eq!(meta.sitename.as_deref(), Some("Example Site"));
        assert_eq!(meta.date.as_deref(), Some("2025-03-04"));
    }

    #[test]
    fn extract_strips_links_by_default_and_keeps_them_when_asked() {
        let (plain, _) = extract_markdown(PAGE, false, false).unwrap();
        assert!(plain.contains("a link"));
        assert!(!plain.contains("https://example.com"), "link url leaked: {plain}");

        let (linked, _) = extract_markdown(PAGE, false, true).unwrap();
        assert!(linked.contains("https://example.com"), "link url missing: {linked}");
    }

    #[test]
    fn extract_falls_back_to_title_tag_when_no_og_title() {
        let html = "<html><head><title>Only Title</title></head><body><article><p>Body text here.</p></article></body></html>";
        let (_, meta) = extract_markdown(html, false, false).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Only Title"));
    }

    #[test]
    fn extract_returns_none_for_a_page_with_no_body_content() {
        assert!(extract_markdown("<html><head><title>t</title></head><body></body></html>", false, false).is_none());
        assert!(extract_markdown("", false, false).is_none());
    }

    #[test]
    fn favor_precision_declines_the_body_wide_fallback() {
        // No <article>/<main>/#content — recall mode falls back to <body>,
        // precision mode declines and reports nothing extractable.
        let html = "<html><body><p>Loose prose with no semantic container.</p></body></html>";
        assert!(extract_markdown(html, false, false).is_some(), "recall mode should extract");
        assert!(extract_markdown(html, true, false).is_none(), "precision mode should decline");
    }

    #[test]
    fn render_reports_pagination_metadata_and_next_offset() {
        let html = format!(
            "<html><head><title>T</title></head><body><article>{}</article></body></html>",
            (0..20).map(|i| format!("<p>Paragraph number {i} with some filler text.</p>")).collect::<String>()
        );
        let out = render_scrape_result(&html, "u", "fu", "text/html", None, "markdown", 0, Some(120), false, false);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "success");
        assert_eq!(v["truncated"], json!(true));
        let next = v["metadata"]["next_offset"].as_u64().expect("next_offset present");
        assert!(next > 0);
        assert_eq!(v["metadata"]["offset"], json!(0));
        assert_eq!(v["metadata"]["final_url"], json!("fu"));

        // Paging from next_offset returns different content.
        let page2 = render_scrape_result(
            &html, "u", "fu", "text/html", None, "markdown", next as usize, Some(120), false, false,
        );
        let v2: Value = serde_json::from_str(&page2).unwrap();
        assert_ne!(v["data"], v2["data"]);
    }

    #[test]
    fn render_with_a_query_filters_and_reports_match_count() {
        let html = "<html><body><article>\
            <p>Alpha content about zebras.</p>\
            <p>Beta content about aardvarks.</p>\
            <p>Gamma content about zebras again.</p>\
            </article></body></html>";
        let out = render_scrape_result(html, "u", "u", "text/html", Some("zebras"), "markdown", 0, None, false, false);
        let v: Value = serde_json::from_str(&out).unwrap();
        let data = v["data"].as_str().unwrap();
        assert!(data.contains("zebras"));
        assert!(!data.contains("aardvarks"), "non-matching block leaked: {data}");
        assert_eq!(v["metadata"]["total_matches"], json!(2));
        assert_eq!(v["metadata"]["filtered_by_query"], json!("zebras"));
    }

    #[test]
    fn render_with_a_no_match_query_says_so_without_fabricating_data() {
        let html = "<html><body><article><p>Alpha.</p><p>Beta.</p></article></body></html>";
        let out = render_scrape_result(
            html, "u", "u", "text/html", Some("zzzznonexistent"), "markdown", 0, None, false, false,
        );
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["message"].as_str().unwrap().contains("No direct matches"));
        assert_eq!(v["metadata"]["total_matches"], json!(0));
        // The "no match" notice is a message, never spliced into data.
        assert!(!v["data"].as_str().unwrap().contains("No direct matches"));
    }

    #[test]
    fn render_reports_scrape_empty_for_an_unextractable_page() {
        let out = render_scrape_result(
            "<html><body></body></html>", "u", "u", "text/html", None, "markdown", 0, None, false, false,
        );
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error_code"], "SCRAPE_EMPTY");
        // Must get the scrape-specific hint, not the search one.
        assert!(v["hint"].as_str().unwrap().contains("lean_web_search"));
    }

    #[test]
    fn pdf_filename_sanitizes_and_forces_extension() {
        assert_eq!(pdf_filename_for("https://e.com/docs/my report.pdf"), "my_report.pdf");
        assert_eq!(pdf_filename_for("https://e.com/paper"), "paper.pdf");
        assert_eq!(pdf_filename_for("https://e.com/"), "downloaded.pdf");
    }

    #[test]
    fn markdown_to_text_strips_headings_emphasis_links_lists_and_tables() {
        let md = "# Big Title\n\nSome **bold** and *italic* text with a [link](https://e.com/a).\n\n- one\n- two\n\n| A | B |\n|---|--:|\n| 1 | 2 |\n```\nignore\n```";
        let text = markdown_to_text(md);
        assert!(text.contains("Big Title"), "heading text kept, got: {text}");
        assert!(!text.contains('#'));

        assert!(text.contains("Some bold and italic text with a link"), "got: {text}");
        assert!(!text.contains("https://e.com/a"), "link url leaked");
        assert!(!text.contains("**"));
        assert!(!text.contains('|'), "table pipes leaked: {text}");
        assert!(!text.contains("```"), "code fence leaked");
        assert!(text.contains("ignore"), "fence content should remain");

        let lower = text.lines().map(str::trim).collect::<Vec<_>>();
        assert!(lower.contains(&"A   B"), "table header not rendered: {lower:?}");
        assert!(lower.contains(&"1   2"), "table row not rendered: {lower:?}");
    }

    #[test]
    fn render_text_mode_returns_plain_text_instead_of_markdown() {
        let html = "<html><body><article><h1>Hello</h1><p>Tail text.</p></article></body></html>";

        let out_md = render_scrape_result(html, "u", "u", "text/html", None, "markdown", 0, None, false, false);
        let vm: Value = serde_json::from_str(&out_md).unwrap();
        assert_eq!(vm["status"], "success");
        assert!(vm["data"].as_str().unwrap().contains("# Hello"), "md mode keeps heading marker");

        let out_text = render_scrape_result(html, "u", "u", "text/html", None, "text", 0, None, false, false);
        let vt: Value = serde_json::from_str(&out_text).unwrap();
        assert_eq!(vt["status"], "success");
        let data = vt["data"].as_str().unwrap();
        assert!(data.contains("Hello"), "heading text kept: {data}");
        assert!(data.contains("Tail text."), "body kept: {data}");
        assert!(!data.contains('#'), "heading marker stripped in text mode: {data}");
    }
}
