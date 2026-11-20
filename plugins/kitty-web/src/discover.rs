//! Links a page offers to *other* forms of its content.
//!
//! Two kinds, both read from the HTML alone (pure; no fetching here):
//!
//! - **Document candidates**: files the page points at — a
//!   `citation_pdf_url`, a link to a `.pdf`, a URL buried in a script or a
//!   data attribute, or the redirect target of a lead-capture form. A report
//!   page that asks for an email address very often still names its PDF
//!   somewhere in the markup; this is how the model gets to see it.
//! - **Alternate versions**: an AMP page, a WordPress REST record, an
//!   RSS/Atom feed — cheaper, script-free renderings `scrape.rs` tries when
//!   the page itself extracts to little.

use std::collections::HashSet;
use std::sync::OnceLock;

use scraper::{Html, Selector};
use url::Url;

use crate::scrape::download_kind;

/// Document candidates returned per page, at most.
pub const MAX_DOCUMENT_CANDIDATES: usize = 8;

/// A file the page points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocCandidate {
    pub url: String,
    pub label: String,
    /// `citation_meta`, `link`, `embedded` or `form_redirect`.
    pub source: &'static str,
    /// The document extension, or `None` for a redirect target whose type
    /// is unknown until fetched.
    pub file_type: Option<&'static str>,
}

/// Everything `discover` found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageLinks {
    pub documents: Vec<DocCandidate>,
    pub amp: Option<String>,
    pub wp_json: Option<String>,
    pub feeds: Vec<String>,
}

/// Find the document candidates and alternate versions `html` (served from
/// `base`) points at.
pub fn discover(html: &str, base: &str) -> PageLinks {
    let Ok(base_url) = Url::parse(base) else {
        return PageLinks::default();
    };
    let document = Html::parse_document(html);
    let page_key = key(base);
    let title_words = title_words(&document);

    let mut found: Vec<(u8, DocCandidate)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut add = |rank: u8, raw: &str, label: String, source: &'static str, require_doc: bool| {
        let Some(url) = resolve(&base_url, raw) else {
            return;
        };
        let file_type = doc_ext(&url);
        if require_doc && file_type.is_none() {
            return;
        }
        let k = key(&url);
        if k == page_key || !seen.insert(k) {
            return;
        }
        let label = if label.trim().is_empty() {
            file_name(&url)
        } else {
            clip(label.trim(), 120)
        };
        found.push((
            rank,
            DocCandidate {
                url,
                label,
                source,
                file_type,
            },
        ));
    };

    // 1. Scholarly/report metadata naming the full text outright.
    for name in [
        "citation_pdf_url",
        "eprints.document_url",
        "bepress_citation_pdf_url",
    ] {
        let sel = Selector::parse(&format!(r#"meta[name="{name}"]"#)).expect("static selector");
        for m in document.select(&sel) {
            if let Some(c) = m.value().attr("content") {
                add(0, c, String::new(), "citation_meta", false);
            }
        }
    }
    let sel = Selector::parse(r#"link[type="application/pdf"]"#).expect("static selector");
    for l in document.select(&sel) {
        if let Some(h) = l.value().attr("href") {
            let label = l.value().attr("title").unwrap_or("").to_string();
            add(0, h, label, "citation_meta", false);
        }
    }

    // 2. Anchors to a document file.
    let sel = Selector::parse("a[href]").expect("static selector");
    for a in document.select(&sel) {
        let href = a.value().attr("href").unwrap_or("");
        let text: String = a.text().collect::<Vec<_>>().join(" ");
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let label = if text.is_empty() {
            a.value().attr("title").unwrap_or("").to_string()
        } else {
            text
        };
        add(1, href, label, "link", true);
    }

    // 3. Any other attribute whose value is a document path (data-file,
    //    hidden inputs, onclick handlers that just hold a URL, …), and
    //    lead-form redirect fields.
    let sel = Selector::parse("*").expect("static selector");
    for el in document.select(&sel) {
        let tag = el.value().name();
        for (attr, value) in el.value().attrs() {
            if tag == "a" && attr == "href" {
                continue;
            }
            let v = value.trim();
            if v.len() > 2048 {
                continue;
            }
            if doc_ext(v).is_some() || (v.starts_with("http") && doc_ext_in(v).is_some()) {
                add(2, v, String::new(), "embedded", true);
            } else if tag == "input" && attr == "value" && is_redirect_field(&el) {
                add(3, v, String::new(), "form_redirect", false);
            }
        }
    }

    // 4. Absolute document URLs and redirect/download keys inside scripts
    //    (JSON escapes `/` as `\/`, so unescape first).
    let sel = Selector::parse("script").expect("static selector");
    for s in document.select(&sel) {
        let text = s.text().collect::<String>().replace("\\/", "/");
        for m in absolute_url_re().find_iter(&text) {
            add(2, m.as_str(), String::new(), "embedded", true);
        }
        for caps in keyed_url_re().captures_iter(&text) {
            let key = caps.get(1).map_or("", |m| m.as_str()).to_ascii_lowercase();
            let value = caps.get(2).map_or("", |m| m.as_str());
            let is_download = ["download", "file", "asset", "pdf", "document"]
                .iter()
                .any(|w| key.contains(w));
            if is_download {
                add(2, value, String::new(), "embedded", false);
            } else {
                add(3, value, String::new(), "form_redirect", false);
            }
        }
    }

    // Rank, then prefer candidates whose label or file name shares words
    // with the page's title — on a report page that is the report itself,
    // not the privacy policy PDF in the footer.
    found.sort_by(|(ra, a), (rb, b)| {
        ra.cmp(rb).then_with(|| {
            overlap(&title_words, &format!("{} {}", b.label, b.url))
                .cmp(&overlap(&title_words, &format!("{} {}", a.label, a.url)))
        })
    });
    let documents = found
        .into_iter()
        .map(|(_, c)| c)
        .take(MAX_DOCUMENT_CANDIDATES)
        .collect();

    PageLinks {
        documents,
        amp: link_href(&document, &base_url, |rel, _| {
            rel.split_whitespace()
                .any(|r| r.eq_ignore_ascii_case("amphtml"))
        }),
        wp_json: link_href(&document, &base_url, |rel, ty| {
            rel.eq_ignore_ascii_case("alternate") && ty == "application/json"
        })
        .filter(|h| h.contains("/wp-json/")),
        feeds: link_hrefs(&document, &base_url, |rel, ty| {
            rel.eq_ignore_ascii_case("alternate")
                && (ty == "application/rss+xml" || ty == "application/atom+xml")
        })
        .into_iter()
        .take(2)
        .collect(),
    }
}

fn link_hrefs(document: &Html, base: &Url, want: impl Fn(&str, &str) -> bool) -> Vec<String> {
    let sel = Selector::parse("link[href]").expect("static selector");
    document
        .select(&sel)
        .filter(|l| {
            let rel = l.value().attr("rel").unwrap_or("");
            let ty = l
                .value()
                .attr("type")
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            want(rel, &ty)
        })
        .filter_map(|l| resolve(base, l.value().attr("href").unwrap_or("")))
        .collect()
}

fn link_href(document: &Html, base: &Url, want: impl Fn(&str, &str) -> bool) -> Option<String> {
    link_hrefs(document, base, want).into_iter().next()
}

fn absolute_url_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r#"https?://[^\s"'<>\\()\[\]{}|^`]+"#).expect("static regex")
    })
}

/// `"redirectUrl": "…"`, `thankYouUrl = '…'`, `pdf_url: "…"` and kin.
fn keyed_url_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r#"(?i)["']?((?:redirect|followup|follow_up|thankyou|thank_you|success|return|download|file|asset|pdf|document)_?url)["']?\s*[:=]\s*["']([^"'\s]{4,2048})["']"#,
        )
        .expect("static regex")
    })
}

/// A hidden form field that holds where the form sends you afterwards.
fn is_redirect_field(input: &scraper::ElementRef) -> bool {
    let name = input
        .value()
        .attr("name")
        .or_else(|| input.value().attr("id"))
        .unwrap_or("")
        .to_ascii_lowercase();
    [
        "redirect",
        "returl",
        "return_url",
        "thank",
        "success",
        "followup",
        "destination",
        "next",
    ]
    .iter()
    .any(|w| name.contains(w))
        && input
            .value()
            .attr("value")
            .is_some_and(|v| v.starts_with("http") || v.starts_with('/'))
}

/// An absolute http(s) URL for `raw` against `base`, fragment dropped.
fn resolve(base: &Url, raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty()
        || raw.starts_with('#')
        || raw.starts_with("javascript:")
        || raw.starts_with("mailto:")
        || raw.starts_with("data:")
    {
        return None;
    }
    let mut u = base.join(raw).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    u.set_fragment(None);
    Some(u.to_string())
}

/// The document extension of a URL's path, if it names one this server can
/// download for a reader.
fn doc_ext(url: &str) -> Option<&'static str> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    download_kind(path, "")
        .map(|k| k.ext)
        .filter(|e| matches!(*e, "pdf" | "docx" | "xlsx" | "xlsm" | "xls" | "csv"))
}

/// Same, but for a URL whose document name sits in its query string
/// (`/download?file=report.pdf`).
fn doc_ext_in(url: &str) -> Option<&'static str> {
    let lower = url.to_ascii_lowercase();
    [".pdf", ".docx", ".xlsx"]
        .iter()
        .find(|ext| {
            lower.match_indices(*ext).any(|(i, _)| {
                !lower[i + ext.len()..].starts_with(|c: char| c.is_ascii_alphanumeric())
            })
        })
        .map(|ext| match *ext {
            ".pdf" => "pdf",
            ".docx" => "docx",
            _ => "xlsx",
        })
}

fn key(url: &str) -> String {
    crate::search::normalize_url_key(url)
}

fn file_name(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let name = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(path);
    clip(name, 120)
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    }
}

fn title_words(document: &Html) -> HashSet<String> {
    let sel = Selector::parse("title, h1").expect("static selector");
    let text: String = document
        .select(&sel)
        .flat_map(|e| e.text())
        .collect::<Vec<_>>()
        .join(" ");
    words(&text)
}

/// Lower-cased words of three letters or more.
pub fn words(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .map(str::to_lowercase)
        .collect()
}

fn overlap(title: &HashSet<String>, text: &str) -> usize {
    words(text).intersection(title).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://example.org/reports/state-of-ports-2026";

    #[test]
    fn citation_meta_comes_first_then_links_ranked_by_title_overlap() {
        let html = r#"<html><head><title>State of Ports 2026</title>
            <meta name="citation_pdf_url" content="/files/ports-2026-full.pdf"></head>
            <body><a href="/legal/privacy.pdf">Privacy</a>
            <a href="/files/state-of-ports-2026-summary.pdf">State of Ports summary</a></body></html>"#;
        let links = discover(html, BASE);
        let urls: Vec<&str> = links.documents.iter().map(|d| d.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://example.org/files/ports-2026-full.pdf",
                "https://example.org/files/state-of-ports-2026-summary.pdf",
                "https://example.org/legal/privacy.pdf",
            ]
        );
        assert_eq!(links.documents[0].source, "citation_meta");
        assert_eq!(links.documents[1].label, "State of Ports summary");
        assert_eq!(links.documents[1].file_type, Some("pdf"));
    }

    #[test]
    fn urls_hidden_in_scripts_and_attributes_are_found() {
        let html = r#"<html><body>
            <div class="gate" data-asset="https://cdn.example.net/assets/ports-report.pdf?v=3"></div>
            <script>var cfg = {"downloadUrl":"https:\/\/cdn.example.net\/dl\/ports.xlsx"};
              hbspt.forms.create({redirectUrl: "https://example.org/thank-you-ports"});</script>
            <form><input type="hidden" name="retURL" value="https://example.org/ports-download"></form>
            </body></html>"#;
        let links = discover(html, BASE);
        let by_url = |u: &str| links.documents.iter().find(|d| d.url == u).cloned();
        assert_eq!(
            by_url("https://cdn.example.net/assets/ports-report.pdf?v=3")
                .unwrap()
                .source,
            "embedded"
        );
        assert_eq!(
            by_url("https://cdn.example.net/dl/ports.xlsx")
                .unwrap()
                .file_type,
            Some("xlsx")
        );
        let redirect = by_url("https://example.org/thank-you-ports").unwrap();
        assert_eq!(redirect.source, "form_redirect");
        assert_eq!(redirect.file_type, None);
        assert!(by_url("https://example.org/ports-download").is_some());
    }

    #[test]
    fn plain_page_links_are_not_documents() {
        let html = r##"<html><body><a href="/about">About</a><a href="mailto:x@y.z">Mail</a>
            <a href="#top">Top</a><a href="https://example.org/page.html">Page</a></body></html>"##;
        assert!(discover(html, BASE).documents.is_empty());
    }

    #[test]
    fn alternate_versions_are_found() {
        let html = r#"<html><head>
            <link rel="amphtml" href="/reports/state-of-ports-2026/amp">
            <link rel="alternate" type="application/json" href="https://example.org/wp-json/wp/v2/posts/42">
            <link rel="alternate" type="application/rss+xml" href="/feed/">
            </head><body></body></html>"#;
        let links = discover(html, BASE);
        assert_eq!(
            links.amp.as_deref(),
            Some("https://example.org/reports/state-of-ports-2026/amp")
        );
        assert_eq!(
            links.wp_json.as_deref(),
            Some("https://example.org/wp-json/wp/v2/posts/42")
        );
        assert_eq!(links.feeds, ["https://example.org/feed/"]);
    }

    #[test]
    fn a_document_named_in_a_query_string_counts() {
        assert_eq!(
            doc_ext_in("https://x.org/dl?file=Report.PDF&t=1"),
            Some("pdf")
        );
        assert_eq!(doc_ext_in("https://x.org/pdfs/viewer"), None);
    }
}
