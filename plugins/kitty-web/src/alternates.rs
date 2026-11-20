//! Parsing the script-free versions of a page `scrape.rs` falls back to:
//! a WordPress REST record, an RSS/Atom feed entry, and the Wayback
//! Machine's availability answer. Pure; the fetching is in `scrape.rs`.

use std::sync::OnceLock;

use serde_json::Value;

/// A WordPress `/wp-json/wp/v2/posts/{id}` (or pages) record as an HTML
/// page: its rendered title and content.
pub fn wordpress_html(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let content = v.pointer("/content/rendered").and_then(Value::as_str)?;
    if content.trim().is_empty() {
        return None;
    }
    let title = v
        .pointer("/title/rendered")
        .and_then(Value::as_str)
        .unwrap_or("");
    Some(page(title, content))
}

/// The entry for `page_url` in an RSS or Atom feed, as an HTML page: its
/// full content (`content:encoded`, Atom `content`) or, failing that, its
/// description/summary.
///
/// Feeds are regular enough that a tolerant regex reading beats pulling in
/// an XML parser for this one job; anything unrecognised just yields `None`.
pub fn feed_entry_html(xml: &str, page_url: &str) -> Option<String> {
    let want = crate::search::normalize_url_key(page_url);
    for entry in entry_re().find_iter(xml) {
        let entry = entry.as_str();
        let links = entry_links(entry);
        if !links
            .iter()
            .any(|l| crate::search::normalize_url_key(l) == want)
        {
            continue;
        }
        let body = ["content:encoded", "content", "description", "summary"]
            .iter()
            .find_map(|tag| element_text(entry, tag))
            .filter(|b| !b.trim().is_empty())?;
        let title = element_text(entry, "title").unwrap_or_default();
        return Some(page(&decode_text(&title), &body));
    }
    None
}

/// From a `https://archive.org/wayback/available` answer: the closest
/// snapshot's raw-content URL (the `id_` form, which serves the page as
/// archived, without the Wayback toolbar) and its timestamp.
pub fn wayback_snapshot(body: &str, page_url: &str) -> Option<(String, String)> {
    let v: Value = serde_json::from_str(body).ok()?;
    let closest = v.pointer("/archived_snapshots/closest")?;
    if closest.get("available").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    if closest
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.starts_with('2'))
    {
        return None;
    }
    let ts = closest.get("timestamp").and_then(Value::as_str)?;
    if ts.len() < 8 || !ts.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((
        format!("https://web.archive.org/web/{ts}id_/{page_url}"),
        ts.to_string(),
    ))
}

/// `20240131120000` as `2024-01-31`.
pub fn wayback_date(ts: &str) -> String {
    if ts.len() >= 8 {
        format!("{}-{}-{}", &ts[0..4], &ts[4..6], &ts[6..8])
    } else {
        ts.to_string()
    }
}

fn page(title: &str, content: &str) -> String {
    let title = title.trim();
    let escaped: String = title
        .chars()
        .map(|c| match c {
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '&' => "&amp;".to_string(),
            c => c.to_string(),
        })
        .collect();
    // An already-escaped title (WordPress sends `&#8217;`) must not be
    // escaped twice; it is entity text, which HTML reads correctly as-is.
    let shown = if title.contains("&#") || title.contains("&amp;") {
        title.to_string()
    } else {
        escaped
    };
    format!(
        "<html><head><title>{shown}</title></head><body><article><h1>{shown}</h1>{content}</article></body></html>"
    )
}

fn entry_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?is)<(item|entry)\b[^>]*>.*?</(item|entry)>").expect("static regex")
    })
}

/// An entry's links: RSS `<link>text</link>` and `<guid>`, Atom
/// `<link href="…">`.
fn entry_links(entry: &str) -> Vec<String> {
    static HREF: OnceLock<regex::Regex> = OnceLock::new();
    let href = HREF.get_or_init(|| {
        regex::Regex::new(r#"(?is)<link\b[^>]*\bhref\s*=\s*["']([^"']+)["']"#)
            .expect("static regex")
    });
    let mut out: Vec<String> = href
        .captures_iter(entry)
        .filter_map(|c| c.get(1).map(|m| decode_text(m.as_str())))
        .collect();
    for tag in ["link", "guid", "feedburner:origLink"] {
        if let Some(t) = element_text(entry, tag) {
            let t = decode_text(&t);
            if t.starts_with("http") {
                out.push(t.trim().to_string());
            }
        }
    }
    out
}

/// The inner text of the first `<tag …>…</tag>` in `xml`, CDATA unwrapped
/// (CDATA content is markup as-is; otherwise the text is entity-escaped
/// markup, which is decoded once).
fn element_text(xml: &str, tag: &str) -> Option<String> {
    let lower = xml.to_ascii_lowercase();
    let open = format!("<{}", tag.to_ascii_lowercase());
    let mut from = 0;
    while let Some(i) = lower[from..].find(&open) {
        let start = from + i;
        let after = start + open.len();
        // `<content` must not match `<contentType`.
        let next = lower[after..].chars().next()?;
        if !(next == '>' || next.is_whitespace() || next == '/') {
            from = after;
            continue;
        }
        let tag_end = after + lower[after..].find('>')?;
        if lower[..tag_end].ends_with('/') {
            from = tag_end;
            continue;
        }
        let close = format!("</{}>", tag.to_ascii_lowercase());
        let end = tag_end + 1 + lower[tag_end + 1..].find(&close)?;
        let inner = xml[tag_end + 1..end].trim();
        if let Some(cdata) = inner
            .strip_prefix("<![CDATA[")
            .and_then(|s| s.strip_suffix("]]>"))
        {
            return Some(cdata.to_string());
        }
        return Some(decode_text(inner));
    }
    None
}

/// Decode XML/HTML entities in a text run.
fn decode_text(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let frag = scraper::Html::parse_fragment(&format!("<p>{s}</p>"));
    frag.root_element().text().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wordpress_record_becomes_a_page() {
        let body = r#"{"id":42,"title":{"rendered":"Port report"},"content":{"rendered":"<p>Body text.</p>"}}"#;
        let html = wordpress_html(body).unwrap();
        assert!(html.contains("<h1>Port report</h1><p>Body text.</p>"));
        assert!(wordpress_html(r#"{"content":{"rendered":""}}"#).is_none());
    }

    #[test]
    fn rss_entry_matching_the_page_is_found_with_full_content() {
        let xml = r#"<rss><channel>
            <item><title>Other</title><link>https://ex.org/other/</link><description>no</description></item>
            <item><title>Ports &amp; harbours</title><link>https://www.ex.org/ports/</link>
              <description>Short teaser</description>
              <content:encoded><![CDATA[<p>The full article.</p>]]></content:encoded></item>
            </channel></rss>"#;
        let html = feed_entry_html(xml, "https://ex.org/ports").unwrap();
        assert!(html.contains("<p>The full article.</p>"), "{html}");
        assert!(html.contains("Ports &amp; harbours"));
        assert!(feed_entry_html(xml, "https://ex.org/missing").is_none());
    }

    #[test]
    fn atom_entry_with_escaped_content_is_decoded() {
        let xml = r#"<feed><entry><title>A</title><link rel="alternate" href="https://ex.org/a"/>
            <content type="html">&lt;p&gt;Atom body.&lt;/p&gt;</content></entry></feed>"#;
        let html = feed_entry_html(xml, "https://ex.org/a").unwrap();
        assert!(html.contains("<p>Atom body.</p>"), "{html}");
    }

    #[test]
    fn wayback_answer_gives_the_raw_snapshot_url() {
        let body = r#"{"archived_snapshots":{"closest":{"status":"200","available":true,
            "url":"http://web.archive.org/web/20240131120000/https://ex.org/a","timestamp":"20240131120000"}}}"#;
        let (url, ts) = wayback_snapshot(body, "https://ex.org/a").unwrap();
        assert_eq!(
            url,
            "https://web.archive.org/web/20240131120000id_/https://ex.org/a"
        );
        assert_eq!(wayback_date(&ts), "2024-01-31");
        assert!(wayback_snapshot(r#"{"archived_snapshots":{}}"#, "https://ex.org/a").is_none());
        let not_ok = body.replace("\"200\"", "\"404\"");
        assert!(wayback_snapshot(&not_ok, "https://ex.org/a").is_none());
    }
}
