//! `lean_web_search` / `lean_web_search_read_chunk` — Rust port of
//! `kitty_docs_web.py`'s count-tiered Brave/DuckDuckGo search.
//!
//! Tool names and response shapes are kept identical to the Python original:
//! adaptive-pathway keys learned routing preferences on the literal tool-name
//! string, and the model's prompt-visible contract is the JSON envelope, so
//! both are load-bearing (see `docs/PLUGINS.md`).
//!
//! Tiering, unchanged from Python:
//! - `count <= NORMAL_MAX_COUNT` (5): Brave if configured, DuckDuckGo only as
//!   a fallback *on Brave failure*. Inline, full detail.
//! - `count <= EXPANDED_MAX_COUNT` (10): Brave AND DuckDuckGo queried
//!   concurrently regardless of whether Brave succeeds — "expansion" means
//!   broader source coverage, not lexical query variants. Still inline.
//! - `count > EXPANDED_MAX_COUNT`: same dual-engine fetch, but the full set is
//!   offloaded to disk and a compact keyword index is returned instead.
//!
//! Every mode offloads and returns a `search_id`, so full detail is always one
//! `lean_web_search_read_chunk` call away.
//!
//! The one unavoidable deviation from Python: `ddgs` has no Rust equivalent,
//! so `ddg_query` scrapes DuckDuckGo's own no-JS HTML endpoint directly. That
//! is what `ddgs` does under the hood too, but it means DuckDuckGo markup
//! changes are now this crate's problem — `parse_ddg_html` is deliberately
//! tolerant (see its doc comment) and pinned by fixture tests.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::envelope::{error_response, success_response};

pub const NORMAL_MAX_COUNT: usize = 5;
pub const EXPANDED_MAX_COUNT: usize = 10;
pub const MAX_COUNT: usize = 50;
pub const KEYWORDS_PER_ITEM: usize = 5;
pub const MANIFEST_TITLE_MAX_CHARS: usize = 60;
pub const INLINE_SNIPPET_MAX_CHARS: usize = 320;
pub const INLINE_RESPONSE_MAX_CHARS: usize = 8000;
pub const READ_CHUNK_MAX_CHARS: usize = 20000;
pub const MAX_OFFLOAD_FILES: usize = 20;
pub const MAX_RATE_LIMIT_RETRIES: u32 = 2;
pub const BASE_BACKOFF_SECONDS: f64 = 1.5;
/// `lean_web_search_read_chunk` returns at most this many ids per call.
pub const READ_CHUNK_MAX_IDS: usize = 5;

const BRAVE_ENDPOINT: &str = "https://api.search.brave.com/res/v1/llm/context";
/// DuckDuckGo's no-JS HTML endpoint — the same one `ddgs` drives.
const DDG_ENDPOINT: &str = "https://html.duckduckgo.com/html/";

/// One search result, in the shape the Python dicts used. `snippet_full` is
/// offload-only and is never serialized inline (see `inline_view`).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchItem {
    pub id: usize,
    pub title: String,
    pub domain: String,
    pub url: String,
    pub date: Option<String>,
    pub snippet: String,
    pub snippet_full: String,
    pub engine: String,
}

impl SearchItem {
    /// Inline projection: drops `snippet_full` so the uncapped text can never
    /// leak into an inline response by omission (Python's `_inline_view`).
    fn to_inline_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("title".into(), json!(self.title));
        m.insert("domain".into(), json!(self.domain));
        m.insert("url".into(), json!(self.url));
        m.insert("date".into(), json!(self.date));
        m.insert("snippet".into(), json!(self.snippet));
        m.insert("engine".into(), json!(self.engine));
        m.insert("id".into(), json!(self.id));
        Value::Object(m)
    }

    /// Offload projection: everything, including `snippet_full`.
    fn to_stored_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("title".into(), json!(self.title));
        m.insert("domain".into(), json!(self.domain));
        m.insert("url".into(), json!(self.url));
        m.insert("date".into(), json!(self.date));
        m.insert("snippet".into(), json!(self.snippet));
        m.insert("snippet_full".into(), json!(self.snippet_full));
        m.insert("engine".into(), json!(self.engine));
        m.insert("id".into(), json!(self.id));
        Value::Object(m)
    }

    fn from_stored_json(v: &Value) -> Option<Self> {
        Some(Self {
            id: v.get("id")?.as_u64()? as usize,
            title: v.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            domain: v.get("domain").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            url: v.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            date: v.get("date").and_then(|x| x.as_str()).map(|s| s.to_string()),
            snippet: v.get("snippet").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            snippet_full: v
                .get("snippet_full")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            engine: v.get("engine").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        })
    }
}

/// Why a Brave call failed. `InvalidQuery` (HTTP 400/422) is a *caller* error
/// and must never trigger a DuckDuckGo fallback — it propagates to the tool
/// boundary as `INVALID_QUERY`. Every other kind is an availability failure
/// and is swallowed into a fallback attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum BraveFailure {
    RateLimitExhausted(String),
    Auth(String),
    Network(String),
    Api(String),
    InvalidQuery(String),
}

impl BraveFailure {
    /// The short string recorded in the response's `metadata.engines` map.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RateLimitExhausted(_) => "rate_limit_exhausted",
            Self::Auth(_) => "auth",
            Self::Network(_) => "network",
            Self::Api(_) => "api",
            Self::InvalidQuery(_) => "invalid_query",
        }
    }

    pub fn detail(&self) -> &str {
        match self {
            Self::RateLimitExhausted(d)
            | Self::Auth(d)
            | Self::Network(d)
            | Self::Api(d)
            | Self::InvalidQuery(d) => d,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (all hermetically testable — no network, no clock, no disk)
// ---------------------------------------------------------------------------

const TRACKING_PARAM_PREFIXES: [&str; 1] = ["utm_"];
const TRACKING_PARAM_EXACT: [&str; 7] = [
    "fbclid", "gclid", "msclkid", "mc_cid", "mc_eid", "ref_src", "igshid",
];

/// Removes known tracking/analytics query params while preserving any
/// parameter that's part of the actual resource address (YouTube's `v=`, a
/// wiki's `?id=`, a forum's `?p=`).
///
/// Deliberately an allowlist-of-junk, not a blanket `?.*$` strip: for any URL
/// where the query string *is* the resource, a blanket strip produces a URL
/// that 404s, and a model that then hands it to `lean_web_scrape` gets a
/// broken-link error it can't explain.
pub fn strip_tracking_params(raw: &str) -> String {
    if raw.is_empty() || !raw.contains('?') {
        return raw.to_string();
    }
    let Ok(mut parsed) = url::Url::parse(raw) else {
        return raw.to_string();
    };
    let kept: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| {
            let lower = k.to_lowercase();
            !(TRACKING_PARAM_PREFIXES.iter().any(|p| lower.starts_with(p))
                || TRACKING_PARAM_EXACT.contains(&lower.as_str()))
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    // Python's `urlencode([])` yields "" and `urlunparse` then emits the URL
    // with the trailing "?" gone entirely; matching that means clearing the
    // query rather than setting it to an empty string.
    let serialized = form_urlencoded_serialize(&kept);
    if serialized.is_empty() {
        parsed.set_query(None);
    } else {
        parsed.set_query(Some(&serialized));
    }
    parsed.to_string()
}

fn form_urlencoded_serialize(pairs: &[(String, String)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(&percent_encode_component(k));
        out.push('=');
        out.push_str(&percent_encode_component(v));
    }
    out
}

/// Matches Python `urlencode`'s default `quote_plus` behavior closely enough
/// for the round-trip these URLs take (they're only ever re-parsed by a
/// browser or `lean_web_scrape`, never byte-compared).
fn percent_encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Lowercased, tracking-stripped URL used only for cross-engine dedup
/// matching — never shown to the caller.
pub fn normalize_url_key(url: &str) -> String {
    let mut cleaned = strip_tracking_params(url).to_lowercase();
    for prefix in ["https://", "http://"] {
        if let Some(rest) = cleaned.strip_prefix(prefix) {
            cleaned = rest.to_string();
            break;
        }
    }
    if let Some(rest) = cleaned.strip_prefix("www.") {
        cleaned = rest.to_string();
    }
    cleaned.trim_end_matches('/').to_string()
}

/// Strips conversational fluff and clamps length before hitting an API.
pub fn apply_query_guardrails(query: &str) -> String {
    let q = query.trim().trim_matches(['"', '\'']).to_string();
    let re = regex::Regex::new(
        r"(?i)^(please\s+)?(search\s+for|find\s+me|look\s+up|what\s+is|who\s+is|where\s+is|can\s+you\s+search\s+for)\s+",
    )
    .expect("static regex is valid");
    let q = re.replace(&q, "").trim().to_string();
    let words: Vec<&str> = q.split_whitespace().collect();
    let q = if words.len() > 50 {
        words[..50].join(" ")
    } else {
        q
    };
    // Python slices by *character*, not byte — `q[:400]` on a str with
    // multi-byte chars would panic here if done with byte indexing.
    q.chars().take(400).collect()
}

/// Returns `(snippet, snippet_full)`. `short` is the inline-appropriate text
/// (Brave's own `sources[].snippet`); `full` is the uncapped page extract.
/// When `short` is empty — about 1 in 9 Brave sources, measured live — fall
/// back to a truncated `full` rather than shipping an empty inline snippet.
pub fn split_snippet(short: &str, full: &str) -> (String, String) {
    let full = full.trim().to_string();
    let short = short.trim();
    let short = if short.is_empty() { full.as_str() } else { short };
    (
        short.chars().take(INLINE_SNIPPET_MAX_CHARS).collect(),
        full.clone(),
    )
}

/// A source's `age` is a list of the same date in several renderings — e.g.
/// `["Saturday, February 22, 2025", "2025-02-22", "526 days ago", ...]`.
/// Prefer the ISO one; fall back to the first entry.
fn source_date(source: &Value) -> Option<String> {
    let age = source.get("age")?;
    if let Some(s) = age.as_str() {
        return if s.is_empty() { None } else { Some(s.to_string()) };
    }
    let arr = age.as_array()?;
    let candidates: Vec<&str> = arr
        .iter()
        .filter_map(|a| a.as_str())
        .filter(|s| !s.is_empty())
        .collect();
    let iso = regex::Regex::new(r"^\d{4}-\d{2}-\d{2}").expect("static regex is valid");
    candidates
        .iter()
        .find(|c| iso.is_match(c))
        .or_else(|| candidates.first())
        .map(|s| s.to_string())
}

/// Brave's `sources` is a **dict keyed by URL** — `{url: {title, hostname,
/// age, snippet}}` — not a list of `{"url": ...}` objects. The list shape is
/// still accepted because it's what the API docs described and what a future
/// revision could go back to; anything else yields an empty map rather than
/// failing, since sources are only ever supplementary to `grounding`.
fn normalize_sources(raw: Option<&Value>) -> HashMap<String, Value> {
    let Some(raw) = raw else {
        return HashMap::new();
    };
    if let Some(obj) = raw.as_object() {
        return obj
            .iter()
            .filter(|(_, v)| v.is_object())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
    }
    if let Some(arr) = raw.as_array() {
        return arr
            .iter()
            .filter(|v| v.is_object())
            .map(|v| {
                (
                    v.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string(),
                    v.clone(),
                )
            })
            .collect();
    }
    HashMap::new()
}

fn domain_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default()
}

/// Extracts a flat result list from Brave's grounding/sources response shape.
///
/// Kept infallible: an unexpected shape yields fewer (or zero) items rather
/// than an error. The Python version wrapped this in `_BraveFailure("api",
/// ...)` specifically so a Brave shape change would degrade to DuckDuckGo
/// instead of taking web search down — here that's achieved more directly by
/// never failing at all, with the empty-result case flowing into the same
/// `ALL_ENGINES_FAILED`/`NO_RESULTS` discrimination at the tool boundary.
pub fn parse_brave_results(payload: &Value) -> Vec<SearchItem> {
    let grounding = payload.get("grounding").cloned().unwrap_or_else(|| json!({}));
    let sources = normalize_sources(payload.get("sources"));
    let mut items = Vec::new();

    let clean = |raw_url: &str| -> (String, String) {
        let clean_url = strip_tracking_params(raw_url);
        let domain = domain_of(&clean_url);
        (clean_url, domain)
    };

    if let Some(generic) = grounding.get("generic").and_then(|g| g.as_array()) {
        for entry in generic {
            let raw_url = entry.get("url").and_then(|u| u.as_str()).unwrap_or("");
            let (clean_url, domain) = clean(raw_url);
            let empty = json!({});
            let source = sources.get(raw_url).unwrap_or(&empty);

            let snippets: Vec<String> = match entry.get("snippets").and_then(|s| s.as_array()) {
                Some(arr) => arr.iter().filter_map(|s| s.as_str()).map(String::from).collect(),
                None => entry
                    .get("snippet")
                    .and_then(|s| s.as_str())
                    .map(|s| vec![s.to_string()])
                    .unwrap_or_default(),
            };
            let joined = snippets.join(" ").trim().to_string();
            let (snippet, snippet_full) = split_snippet(
                source.get("snippet").and_then(|s| s.as_str()).unwrap_or(""),
                &joined,
            );

            items.push(SearchItem {
                id: 0,
                title: entry.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string(),
                domain: if domain.is_empty() {
                    source
                        .get("hostname")
                        .and_then(|h| h.as_str())
                        .unwrap_or("")
                        .to_string()
                } else {
                    domain
                },
                url: clean_url,
                date: source_date(entry).or_else(|| source_date(source)),
                snippet,
                snippet_full,
                engine: "brave".to_string(),
            });
        }
    }

    if let Some(poi) = grounding.get("poi").filter(|p| !p.is_null()) {
        let raw_url = poi.get("url").and_then(|u| u.as_str()).unwrap_or("");
        let (clean_url, domain) = clean(raw_url);
        let desc = poi.get("description").and_then(|d| d.as_str()).unwrap_or("");
        let (snippet, snippet_full) = split_snippet(desc, desc);
        items.push(SearchItem {
            id: 0,
            title: poi.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string(),
            domain,
            url: clean_url,
            date: None,
            snippet,
            snippet_full,
            engine: "brave".to_string(),
        });
    }

    if let Some(map_entries) = grounding.get("map").and_then(|m| m.as_array()) {
        for entry in map_entries {
            let raw_url = entry.get("url").and_then(|u| u.as_str()).unwrap_or("");
            let (clean_url, domain) = clean(raw_url);
            let desc = entry.get("description").and_then(|d| d.as_str()).unwrap_or("");
            let (snippet, snippet_full) = split_snippet(desc, desc);
            items.push(SearchItem {
                id: 0,
                title: entry.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string(),
                domain,
                url: clean_url,
                date: None,
                snippet,
                snippet_full,
                engine: "brave".to_string(),
            });
        }
    }

    items
}

/// Unwraps DuckDuckGo's `/l/?uddg=<percent-encoded target>` redirect wrapper.
/// Returns the input unchanged when it isn't a wrapper, so both the wrapped
/// and (occasionally served) direct-href shapes work.
fn unwrap_ddg_redirect(href: &str) -> String {
    // Protocol-relative (`//duckduckgo.com/l/?...`) is the common shape;
    // `url::Url::parse` needs an absolute URL, so normalize first.
    let absolute = if let Some(rest) = href.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        href.to_string()
    };
    let Ok(parsed) = url::Url::parse(&absolute) else {
        return href.to_string();
    };
    if let Some((_, target)) = parsed.query_pairs().find(|(k, _)| k == "uddg") {
        return target.into_owned();
    }
    absolute
}

/// Parses DuckDuckGo's no-JS HTML result page.
///
/// Deliberately tolerant: it selects on the long-stable `.result__a` /
/// `.result__snippet` class names and skips any result it can't get a usable
/// URL out of, rather than failing the whole parse. A DuckDuckGo markup
/// change should degrade to "fewer/no DuckDuckGo results" (which the caller
/// already handles — Brave still answers, and `ALL_ENGINES_FAILED` covers the
/// total-failure case), never a hard error. Ad results are excluded by
/// skipping `.result--ad` containers.
pub fn parse_ddg_html(html: &str, count: usize) -> Vec<SearchItem> {
    use scraper::{Html, Selector};

    let document = Html::parse_document(html);
    let (Ok(result_sel), Ok(link_sel), Ok(snippet_sel)) = (
        Selector::parse("div.result, div.web-result"),
        Selector::parse("a.result__a"),
        Selector::parse("a.result__snippet, div.result__snippet"),
    ) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for element in document.select(&result_sel) {
        if out.len() >= count {
            break;
        }
        let classes = element.value().attr("class").unwrap_or("");
        if classes.contains("result--ad") || classes.contains("result--news") {
            continue;
        }

        let Some(link) = element.select(&link_sel).next() else {
            continue;
        };
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let target = unwrap_ddg_redirect(href);
        if target.is_empty() || !target.starts_with("http") {
            continue;
        }

        let title = link.text().collect::<String>().trim().to_string();
        let body = element
            .select(&snippet_sel)
            .next()
            .map(|s| s.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        let clean_url = strip_tracking_params(&target);
        let (snippet, snippet_full) = split_snippet(&body, &body);
        out.push(SearchItem {
            id: 0,
            title,
            domain: domain_of(&clean_url),
            url: clean_url,
            date: None,
            snippet,
            snippet_full,
            engine: "duckduckgo".to_string(),
        });
    }
    out
}

/// ~150 common English stopwords, filtered out before frequency-counting a
/// result's title+snippet. Frequency-based (TF), not LSA: at this scale (a
/// handful of short blurbs) there isn't enough text for SVD to find real
/// latent structure beyond what plain term frequency already shows.
const STOPWORDS: &str = "
    a about above after again against all am an and any are aren't as at be because been
    before being below between both but by can't cannot could couldn't did didn't do does
    doesn't doing don't down during each few for from further had hadn't has hasn't have
    haven't having he he'd he'll he's her here here's hers herself him himself his how
    how's i i'd i'll i'm i've if in into is isn't it it's its itself let's me more most
    mustn't my myself no nor not of off on once only or other ought our ours ourselves out
    over own same shan't she she'd she'll she's should shouldn't so some such than that
    that's the their theirs them themselves then there there's these they they'd they'll
    they're they've this those through to too under until up very was wasn't we we'd we'll
    we're we've were weren't what what's when when's where where's which while who who's
    whom why why's will with won't would wouldn't you you'd you'll you're you've your
    yours yourself yourselves new using use used via also into
";

fn stopwords() -> &'static HashSet<&'static str> {
    use std::sync::OnceLock;
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| STOPWORDS.split_whitespace().collect())
}

/// Deterministic, frequency-based keyword extraction.
///
/// Ordering must be reproducible: Python's `Counter` preserves
/// first-insertion order and its sort is stable, so sorting only by `-count`
/// keeps first-appearance order as the tie-break. Rust's `HashMap` has no
/// such guarantee, so first-appearance index is tracked explicitly and used
/// as the tie-break — same output, without depending on map iteration order.
pub fn extract_keywords(text: &str, top_k: usize) -> Vec<String> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"[a-zA-Z][a-zA-Z\-']+").expect("static regex is valid"));

    let lowered = text.to_lowercase();
    let mut counts: HashMap<String, (usize, usize)> = HashMap::new();
    let mut order = 0usize;
    for m in re.find_iter(&lowered) {
        let w = m.as_str();
        if w.chars().count() <= 2 || stopwords().contains(w) {
            continue;
        }
        let entry = counts.entry(w.to_string()).or_insert_with(|| {
            let seen_at = order;
            order += 1;
            (0, seen_at)
        });
        entry.0 += 1;
    }

    let mut ranked: Vec<(String, usize, usize)> =
        counts.into_iter().map(|(w, (c, i))| (w, c, i)).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
    ranked.into_iter().take(top_k).map(|(w, _, _)| w).collect()
}

/// No ranking/scoring across items — order is just merge order (Brave-first,
/// then DuckDuckGo-only). Keywords are mined from `snippet_full`, not the
/// truncated inline `snippet`: the manifest should reflect everything a
/// follow-up `read_chunk` would return, not just the ~320 chars a caller who
/// skips indexing would see.
pub fn build_index(results: &[SearchItem]) -> Value {
    Value::Array(
        results
            .iter()
            .map(|r| {
                let source_text = if r.snippet_full.is_empty() {
                    r.snippet.clone()
                } else {
                    r.snippet_full.clone()
                };
                json!({
                    "id": r.id,
                    "title": r.title.chars().take(MANIFEST_TITLE_MAX_CHARS).collect::<String>(),
                    "domain": r.domain,
                    "engine": r.engine,
                    "keywords": extract_keywords(&format!("{} {}", r.title, source_text), KEYWORDS_PER_ITEM),
                })
            })
            .collect(),
    )
}

fn inline_view(results: &[SearchItem]) -> Value {
    Value::Array(results.iter().map(|r| r.to_inline_json()).collect())
}

/// Merges Brave-first then DuckDuckGo-only results, deduping on
/// `normalize_url_key`.
pub fn merge_dedup(brave: Vec<SearchItem>, ddg: Vec<SearchItem>) -> Vec<SearchItem> {
    let mut seen: HashSet<String> = brave.iter().map(|r| normalize_url_key(&r.url)).collect();
    let mut merged = brave;
    for item in ddg {
        let key = normalize_url_key(&item.url);
        if seen.insert(key) {
            merged.push(item);
        }
    }
    merged
}

/// `"normal" | "expanded" | "expansive"` for a (already clamped) count.
pub fn mode_for_count(count: usize) -> &'static str {
    if count <= NORMAL_MAX_COUNT {
        "normal"
    } else if count <= EXPANDED_MAX_COUNT {
        "expanded"
    } else {
        "expansive"
    }
}

// ---------------------------------------------------------------------------
// Offload store
// ---------------------------------------------------------------------------

/// Sibling to the tool cache dir, not inside it — so a future cache-clear
/// tool can never delete an in-flight search offload. Mirrors the Python
/// constant `SEARCH_STORE_DIR` (`~/.cache/kitty-search-offload`) exactly, so
/// a mixed Python/Rust install shares one store.
pub fn search_store_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache")
        .join("kitty-search-offload")
}

fn offload_path(search_id: &str) -> PathBuf {
    search_store_dir().join(format!("search-{search_id}.json"))
}

fn prune_old_offloads() {
    let dir = search_store_dir();
    if !dir.exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("search-")
        })
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, e.path()))
        })
        .collect();
    // Newest first, so `skip(MAX_OFFLOAD_FILES - 1)` below drops the oldest.
    files.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    // -1: room for the new file about to be written.
    for (_, stale) in files.into_iter().skip(MAX_OFFLOAD_FILES.saturating_sub(1)) {
        let _ = std::fs::remove_file(stale);
    }
}

fn new_search_id() -> String {
    use rand::Rng;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let salt: u16 = rand::thread_rng().gen();
    format!("{millis:x}-{salt:04x}")
}

fn write_offload(search_id: &str, query: &str, results: &[SearchItem]) {
    let dir = search_store_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    prune_old_offloads();
    let payload = json!({
        "search_id": search_id,
        "query": query,
        "results": results.iter().map(|r| r.to_stored_json()).collect::<Vec<_>>(),
    });
    let _ = std::fs::write(
        offload_path(search_id),
        serde_json::to_string(&payload).unwrap_or_default(),
    );
}

// ---------------------------------------------------------------------------
// Network calls
// ---------------------------------------------------------------------------

fn brave_api_key() -> String {
    std::env::var("BRAVE_API_KEY").unwrap_or_default()
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())
}

/// Calls Brave's LLM-context search API with bounded 429 retry/backoff.
async fn brave_query(
    query: &str,
    count: usize,
    search_lang: &str,
    freshness: Option<&str>,
    country: &str,
) -> Result<Vec<SearchItem>, BraveFailure> {
    let key = brave_api_key();
    let client = http_client().map_err(BraveFailure::Network)?;

    for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
        let mut params: Vec<(&str, String)> = vec![
            ("q", query.to_string()),
            ("count", count.clamp(1, 50).to_string()),
            ("search_lang", search_lang.to_string()),
            ("country", country.to_string()),
        ];
        if let Some(f) = freshness {
            params.push(("freshness", f.to_string()));
        }

        let response = client
            .get(BRAVE_ENDPOINT)
            .query(&params)
            .header("X-Subscription-Token", &key)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| BraveFailure::Network(e.to_string()))?;

        let status = response.status();
        if status.as_u16() == 429 {
            if attempt < MAX_RATE_LIMIT_RETRIES {
                let retry_after = response
                    .headers()
                    .get("Retry-After")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<f64>().ok())
                    .map(|v| v.max(0.0));
                let delay = retry_after.unwrap_or_else(|| {
                    use rand::Rng;
                    BASE_BACKOFF_SECONDS * 2f64.powi(attempt as i32)
                        + rand::thread_rng().gen_range(0.0..0.5)
                });
                tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                continue;
            }
            let body = read_error_body(response).await;
            return Err(BraveFailure::RateLimitExhausted(body));
        }

        let body = match crate::scrape::read_body_capped(response, crate::scrape::SCRAPE_MAX_BODY_BYTES).await {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(crate::scrape::BodyReadError::TooLarge) => {
                return Err(BraveFailure::Api("response body exceeded the download cap".into()));
            }
            Err(crate::scrape::BodyReadError::Network(e)) => {
                return Err(BraveFailure::Network(e.to_string()));
            }
        };
        if status.as_u16() == 400 || status.as_u16() == 422 {
            return Err(BraveFailure::InvalidQuery(body));
        }
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(BraveFailure::Auth(body));
        }
        if !status.is_success() {
            return Err(BraveFailure::Api(format!("HTTP {status}: {body}")));
        }

        let payload: Value = serde_json::from_str(&body)
            .map_err(|e| BraveFailure::Api(format!("unparseable response: {e}")))?;
        return Ok(parse_brave_results(&payload));
    }

    Err(BraveFailure::RateLimitExhausted("retries exhausted".into()))
}

/// Reads an error/status body (rate-limit, auth, API-error details) with
/// the same hard cap as real payloads — an error response is still a
/// model-influenced byte stream (audit #112). Oversize bodies degrade to a
/// placeholder rather than a failure, since the status code already carries
/// the outcome.
async fn read_error_body(response: reqwest::Response) -> String {
    match crate::scrape::read_body_capped(response, crate::scrape::SCRAPE_MAX_BODY_BYTES).await {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(crate::scrape::BodyReadError::TooLarge) => "[body exceeded the download cap]".to_string(),
        Err(crate::scrape::BodyReadError::Network(_)) => String::new(),
    }
}

/// Single-shot DuckDuckGo search, no retry — matching the Python original's
/// `_ddg_query` behavior.
async fn ddg_query(query: &str, count: usize) -> Result<Vec<SearchItem>, String> {
    let capped = count.clamp(1, 20);
    let client = http_client()?;
    let response = client
        .post(DDG_ENDPOINT)
        .form(&[("q", query), ("kl", "wt-wt")])
        .header("User-Agent", crate::scrape::SCRAPE_USER_AGENT)
        .header("Accept", "text/html,application/xhtml+xml")
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let html = match crate::scrape::read_body_capped(response, crate::scrape::SCRAPE_MAX_BODY_BYTES).await {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(crate::scrape::BodyReadError::TooLarge) => {
            return Err("response body exceeded the download cap".to_string());
        }
        Err(crate::scrape::BodyReadError::Network(e)) => return Err(e.to_string()),
    };
    Ok(parse_ddg_html(&html, capped))
}

/// `count <= NORMAL_MAX_COUNT`: Brave-first-if-configured, DuckDuckGo only as
/// a failure fallback. `InvalidQuery` is a caller error and propagates
/// directly rather than triggering a fallback.
async fn normal_search(
    query: &str,
    count: usize,
    search_lang: &str,
    freshness: Option<&str>,
    country: &str,
) -> Result<(Vec<SearchItem>, HashMap<String, String>), BraveFailure> {
    let mut diagnostics: HashMap<String, String> = HashMap::from([
        ("brave".to_string(), "not_configured".to_string()),
        ("duckduckgo".to_string(), "not_queried".to_string()),
    ]);

    if !brave_api_key().is_empty() {
        match brave_query(query, count, search_lang, freshness, country).await {
            Ok(results) => {
                diagnostics.insert("brave".into(), "ok".into());
                return Ok((results, diagnostics));
            }
            Err(e) => {
                if matches!(e, BraveFailure::InvalidQuery(_)) {
                    return Err(e);
                }
                diagnostics.insert("brave".into(), e.kind().to_string());
            }
        }
    }

    match ddg_query(query, count).await {
        Ok(results) => {
            diagnostics.insert("duckduckgo".into(), "ok".into());
            Ok((results, diagnostics))
        }
        Err(e) => {
            diagnostics.insert("duckduckgo".into(), format!("failed: {e}"));
            Ok((Vec::new(), diagnostics))
        }
    }
}

/// `count > NORMAL_MAX_COUNT`: queries Brave AND DuckDuckGo concurrently
/// regardless of whether Brave succeeds — this is the "expansion": broader
/// source coverage, not lexical query variants. Brave's `invalid_query` does
/// not abort DuckDuckGo's side here; it's only recorded in diagnostics.
async fn dual_engine_search(
    query: &str,
    count: usize,
    search_lang: &str,
    freshness: Option<&str>,
    country: &str,
) -> (Vec<SearchItem>, HashMap<String, String>) {
    let mut diagnostics: HashMap<String, String> = HashMap::from([
        ("brave".to_string(), "not_configured".to_string()),
        ("duckduckgo".to_string(), "not_queried".to_string()),
    ]);

    let key_configured = !brave_api_key().is_empty();
    let brave_fut = async {
        if key_configured {
            Some(brave_query(query, count, search_lang, freshness, country).await)
        } else {
            None
        }
    };
    let (brave_outcome, ddg_outcome) = tokio::join!(brave_fut, ddg_query(query, count));

    let brave_results = match brave_outcome {
        Some(Ok(r)) => {
            diagnostics.insert("brave".into(), "ok".into());
            r
        }
        Some(Err(e)) => {
            diagnostics.insert("brave".into(), e.kind().to_string());
            Vec::new()
        }
        None => Vec::new(),
    };
    let ddg_results = match ddg_outcome {
        Ok(r) => {
            diagnostics.insert("duckduckgo".into(), "ok".into());
            r
        }
        Err(e) => {
            diagnostics.insert("duckduckgo".into(), format!("failed: {e}"));
            Vec::new()
        }
    };

    (merge_dedup(brave_results, ddg_results), diagnostics)
}

// ---------------------------------------------------------------------------
// Tool entry points
// ---------------------------------------------------------------------------

pub async fn web_search(
    query: &str,
    count: usize,
    search_lang: &str,
    freshness: Option<&str>,
    country: &str,
) -> String {
    let count = count.clamp(1, MAX_COUNT);
    let guarded_q = apply_query_guardrails(query);
    if guarded_q.is_empty() {
        return error_response(
            "EMPTY_QUERY",
            "The search query was empty or contained only whitespace.",
            None,
            None,
        );
    }

    let mode = mode_for_count(count);
    let (mut results, diagnostics) = if mode == "normal" {
        match normal_search(&guarded_q, count, search_lang, freshness, country).await {
            Ok(v) => v,
            Err(e) => {
                // Only reachable for InvalidQuery — every other kind is
                // swallowed into a fallback attempt inside normal_search.
                return error_response(
                    "INVALID_QUERY",
                    "Brave Search API rejected the query parameters.",
                    Some(e.detail()),
                    Some("Simplify the query text or check freshness/country values."),
                );
            }
        }
    } else {
        dual_engine_search(&guarded_q, count, search_lang, freshness, country).await
    };

    if results.is_empty() {
        // "Nothing matched" and "every engine was broken or unreachable" are
        // different problems with different fixes; returning NO_RESULTS for
        // both sends the model off rewording a query that was never the
        // issue. `diagnostics` is the discriminator.
        let any_ok = diagnostics.values().any(|s| s == "ok");
        if !any_ok {
            let detail = serde_json::to_string(&diagnostics).unwrap_or_default();
            return error_response(
                "ALL_ENGINES_FAILED",
                "Every search engine failed; no search was actually performed.",
                Some(&detail),
                Some(
                    "A transient outage or rate limit. Retry once; if it persists the query \
                     itself is not the problem.",
                ),
            );
        }
        return error_response(
            "NO_RESULTS",
            "No results found across the configured search engines.",
            None,
            Some("Try a broader or different query, or increase count."),
        );
    }

    // Ids and the offload write happen for every mode, not just "expansive" —
    // full detail is always one read_chunk call away, so a caller never has
    // to guess in advance whether a search is "big enough" to need it.
    for (idx, item) in results.iter_mut().enumerate() {
        item.id = idx + 1;
    }
    let search_id = new_search_id();
    write_offload(&search_id, &guarded_q, &results);

    let base_metadata = json!({
        "mode": mode,
        "engines": diagnostics,
        "search_id": search_id,
        "query": guarded_q,
        "count": count,
    });

    let shown: Vec<SearchItem> = results.iter().take(count).cloned().collect();

    if mode == "normal" || mode == "expanded" {
        let inline_response = success_response(
            inline_view(&shown),
            None,
            false,
            Some(base_metadata.clone()),
        );
        // Brave's grounding snippets can be page-extract-sized even after the
        // per-result cap in `split_snippet` — this is the hard backstop,
        // independent of mode/count, guaranteeing no reply leaves this
        // function over budget.
        if inline_response.chars().count() <= INLINE_RESPONSE_MAX_CHARS {
            return inline_response;
        }
        let mut meta = base_metadata.as_object().cloned().unwrap_or_default();
        meta.insert("total_results_found".into(), json!(results.len()));
        meta.insert("downgraded_to_index".into(), json!(true));
        meta.insert("inline_chars".into(), json!(inline_response.chars().count()));
        return success_response(build_index(&shown), None, false, Some(Value::Object(meta)));
    }

    let mut meta = base_metadata.as_object().cloned().unwrap_or_default();
    meta.insert("total_results_found".into(), json!(results.len()));
    success_response(build_index(&shown), None, false, Some(Value::Object(meta)))
}

pub fn web_search_read_chunk(search_id: &str, ids: &[i64]) -> String {
    if search_id.contains('/') || search_id.contains('\\') || search_id.contains("..") {
        return error_response(
            "SEARCH_ID_NOT_FOUND",
            "Invalid search_id.",
            None,
            Some("Call lean_web_search again."),
        );
    }

    let path = offload_path(search_id);
    if !path.exists() {
        return error_response(
            "SEARCH_ID_NOT_FOUND",
            &format!("No stored search results for search_id '{search_id}'."),
            None,
            Some(
                "This search_id may have expired (only the 20 most recent searches are \
                 retained) or been mistyped; call lean_web_search again.",
            ),
        );
    }

    let stored: Value = match std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
    {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                "SEARCH_ID_NOT_FOUND",
                &format!("Stored search results are corrupt: {e}"),
                None,
                Some("Call lean_web_search again."),
            );
        }
    };

    let by_id: HashMap<usize, SearchItem> = stored
        .get("results")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(SearchItem::from_stored_json)
                .map(|item| (item.id, item))
                .collect()
        })
        .unwrap_or_default();

    let truncated_ids = ids.len() > READ_CHUNK_MAX_IDS;
    let candidates: Vec<&SearchItem> = ids
        .iter()
        .take(READ_CHUNK_MAX_IDS)
        .filter_map(|i| usize::try_from(*i).ok())
        .filter_map(|i| by_id.get(&i))
        .collect();

    if candidates.is_empty() {
        return error_response(
            "ID_NOT_FOUND",
            "None of the requested ids exist in this search.",
            None,
            Some("Ids come from a lean_web_search response; call it again if unsure."),
        );
    }

    // `snippet_full` is the whole reason this endpoint exists — it becomes
    // each item's `snippet` here, since a caller reading a specific result
    // wants the full text, not the inline-sized preview. Accumulate in id
    // order and stop before READ_CHUNK_MAX_CHARS, measuring the real
    // serialized size rather than estimating: 5 full Brave page extracts can
    // otherwise be a ~40K-char reply on its own.
    let mut matched: Vec<Value> = Vec::new();
    let mut matched_ids: Vec<usize> = Vec::new();
    let mut running_chars = 0usize;
    let mut char_truncated = false;
    for r in candidates {
        let mut item = r.to_inline_json();
        let full = if r.snippet_full.is_empty() {
            r.snippet.clone()
        } else {
            r.snippet_full.clone()
        };
        if let Some(obj) = item.as_object_mut() {
            obj.insert("snippet".into(), json!(full));
        }
        let item_chars = serde_json::to_string(&item).unwrap_or_default().chars().count();
        if !matched.is_empty() && running_chars + item_chars > READ_CHUNK_MAX_CHARS {
            char_truncated = true;
            break;
        }
        matched_ids.push(r.id);
        matched.push(item);
        running_chars += item_chars;
    }

    let mut meta = Map::new();
    meta.insert("search_id".into(), json!(search_id));
    if truncated_ids {
        meta.insert("ids_truncated_to".into(), json!(READ_CHUNK_MAX_IDS));
    }
    if char_truncated {
        meta.insert("ids_returned".into(), json!(matched_ids));
    }

    success_response(
        Value::Array(matched),
        None,
        char_truncated,
        Some(Value::Object(meta)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(url: &str, engine: &str) -> SearchItem {
        SearchItem {
            id: 0,
            title: "t".into(),
            domain: domain_of(url),
            url: url.into(),
            date: None,
            snippet: "s".into(),
            snippet_full: "s".into(),
            engine: engine.into(),
        }
    }

    #[test]
    fn strip_tracking_keeps_resource_params_and_drops_junk() {
        // The exact regression the Python comment calls out: a blanket
        // `?.*$` strip breaks YouTube links.
        let cleaned = strip_tracking_params("https://youtube.com/watch?v=abc123&utm_source=x&fbclid=y");
        assert!(cleaned.contains("v=abc123"), "got {cleaned}");
        assert!(!cleaned.contains("utm_source"));
        assert!(!cleaned.contains("fbclid"));
    }

    #[test]
    fn strip_tracking_drops_query_entirely_when_only_junk() {
        let cleaned = strip_tracking_params("https://example.com/page?utm_source=x");
        assert_eq!(cleaned, "https://example.com/page");
    }

    #[test]
    fn strip_tracking_leaves_queryless_urls_untouched() {
        assert_eq!(
            strip_tracking_params("https://example.com/a/b"),
            "https://example.com/a/b"
        );
    }

    #[test]
    fn normalize_url_key_ignores_scheme_www_and_trailing_slash() {
        assert_eq!(
            normalize_url_key("https://www.Example.com/Path/"),
            normalize_url_key("http://example.com/path")
        );
    }

    #[test]
    fn query_guardrails_strip_conversational_prefixes_and_clamp() {
        assert_eq!(apply_query_guardrails("  please search for rust wasm  "), "rust wasm");
        assert_eq!(apply_query_guardrails("\"what is borrow checker\""), "borrow checker");
        let long = "word ".repeat(80);
        assert_eq!(apply_query_guardrails(&long).split_whitespace().count(), 50);
    }

    #[test]
    fn query_guardrails_clamp_by_chars_not_bytes() {
        // A byte-index slice here would panic mid-codepoint; Python slices
        // by character, so this must too.
        let q = "\u{4e2d}".repeat(500);
        let out = apply_query_guardrails(&q);
        assert_eq!(out.chars().count(), 400);
    }

    #[test]
    fn split_snippet_falls_back_to_full_when_short_is_empty() {
        let (snippet, full) = split_snippet("", "  the full page extract  ");
        assert_eq!(snippet, "the full page extract");
        assert_eq!(full, "the full page extract");
    }

    #[test]
    fn split_snippet_caps_inline_but_never_full() {
        let long = "x".repeat(1000);
        let (snippet, full) = split_snippet("", &long);
        assert_eq!(snippet.chars().count(), INLINE_SNIPPET_MAX_CHARS);
        assert_eq!(full.chars().count(), 1000);
    }

    #[test]
    fn parse_brave_reads_the_dict_keyed_sources_shape() {
        // Brave's `sources` is a dict keyed by URL, not a list — the exact
        // shape confusion the Python docstring documents.
        let payload = json!({
            "grounding": {
                "generic": [{
                    "url": "https://example.com/a?utm_source=brave",
                    "title": "Example A",
                    "snippets": ["first part", "second part"],
                    "age": ["Saturday, February 22, 2025", "2025-02-22"]
                }]
            },
            "sources": {
                "https://example.com/a?utm_source=brave": {
                    "hostname": "example.com",
                    "snippet": "short blurb"
                }
            }
        });
        let items = parse_brave_results(&payload);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Example A");
        assert_eq!(items[0].snippet, "short blurb");
        assert_eq!(items[0].snippet_full, "first part second part");
        assert_eq!(items[0].engine, "brave");
        // ISO rendering preferred over the human one.
        assert_eq!(items[0].date.as_deref(), Some("2025-02-22"));
        // Tracking param stripped from the surfaced URL.
        assert_eq!(items[0].url, "https://example.com/a");
    }

    #[test]
    fn parse_brave_accepts_the_legacy_list_sources_shape() {
        let payload = json!({
            "grounding": {"generic": [{"url": "https://e.com/x", "title": "X", "snippet": "s"}]},
            "sources": [{"url": "https://e.com/x", "snippet": "listed"}]
        });
        let items = parse_brave_results(&payload);
        assert_eq!(items[0].snippet, "listed");
    }

    #[test]
    fn parse_brave_tolerates_a_totally_unexpected_shape() {
        // Must degrade to "no results" (which the caller turns into a
        // DuckDuckGo fallback / ALL_ENGINES_FAILED), never panic.
        assert!(parse_brave_results(&json!({"unexpected": true})).is_empty());
        assert!(parse_brave_results(&json!({"grounding": "not-an-object"})).is_empty());
        assert!(parse_brave_results(&json!(null)).is_empty());
    }

    #[test]
    fn parse_brave_includes_poi_and_map_entries() {
        let payload = json!({
            "grounding": {
                "poi": {"url": "https://p.com", "title": "P", "description": "poi desc"},
                "map": [{"url": "https://m.com", "title": "M", "description": "map desc"}]
            }
        });
        let items = parse_brave_results(&payload);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].snippet, "poi desc");
        assert_eq!(items[1].snippet, "map desc");
    }

    const DDG_FIXTURE: &str = r##"
    <html><body>
      <div class="result results_links results_links_deep web-result">
        <h2 class="result__title">
          <a rel="nofollow" class="result__a"
             href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fone%3Fv%3D1&amp;rut=abc">First Result</a>
        </h2>
        <a class="result__snippet" href="#">The first snippet body.</a>
      </div>
      <div class="result results_links result--ad">
        <h2 class="result__title"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fad.com">Ad</a></h2>
        <a class="result__snippet">Should be skipped.</a>
      </div>
      <div class="result results_links web-result">
        <h2 class="result__title">
          <a class="result__a" href="https://direct.example.org/two">Second Result</a>
        </h2>
        <a class="result__snippet">Second snippet.</a>
      </div>
    </body></html>
    "##;

    #[test]
    fn parse_ddg_unwraps_redirects_skips_ads_and_handles_direct_hrefs() {
        let items = parse_ddg_html(DDG_FIXTURE, 10);
        assert_eq!(items.len(), 2, "ad result must be skipped");
        assert_eq!(items[0].title, "First Result");
        assert_eq!(items[0].url, "https://example.com/one?v=1");
        assert_eq!(items[0].domain, "example.com");
        assert_eq!(items[0].snippet, "The first snippet body.");
        assert_eq!(items[0].engine, "duckduckgo");
        // Direct (non-wrapped) href shape still works.
        assert_eq!(items[1].url, "https://direct.example.org/two");
    }

    #[test]
    fn parse_ddg_respects_count_and_survives_garbage() {
        assert_eq!(parse_ddg_html(DDG_FIXTURE, 1).len(), 1);
        assert!(parse_ddg_html("<html><body>nothing here</body></html>", 10).is_empty());
        assert!(parse_ddg_html("", 10).is_empty());
        assert!(parse_ddg_html("<<<not really html", 10).is_empty());
    }

    #[test]
    fn merge_dedup_prefers_brave_and_drops_equivalent_ddg_urls() {
        let brave = vec![item("https://example.com/a", "brave")];
        let ddg = vec![
            item("https://www.example.com/a/", "duckduckgo"),
            item("https://other.com/b", "duckduckgo"),
        ];
        let merged = merge_dedup(brave, ddg);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].engine, "brave");
        assert_eq!(merged[1].url, "https://other.com/b");
    }

    #[test]
    fn mode_tiers_match_the_documented_thresholds() {
        assert_eq!(mode_for_count(1), "normal");
        assert_eq!(mode_for_count(5), "normal");
        assert_eq!(mode_for_count(6), "expanded");
        assert_eq!(mode_for_count(10), "expanded");
        assert_eq!(mode_for_count(11), "expansive");
    }

    #[test]
    fn extract_keywords_is_deterministic_and_drops_stopwords() {
        let text = "The quick brown fox and the quick brown dog with the fox";
        let first = extract_keywords(text, 5);
        let second = extract_keywords(text, 5);
        assert_eq!(first, second, "must be reproducible across calls");
        assert!(!first.iter().any(|w| w == "the" || w == "and" || w == "with"));
        // "quick"/"brown"/"fox" all appear; counts order them, first-appearance breaks ties.
        assert_eq!(first[0], "quick");
        assert_eq!(first[1], "brown");
        assert_eq!(first[2], "fox");
    }

    #[test]
    fn extract_keywords_skips_short_words() {
        assert!(extract_keywords("go up at ax bee", 5).iter().all(|w| w.len() > 2));
    }

    #[test]
    fn build_index_uses_snippet_full_and_caps_title() {
        let mut r = item("https://e.com", "brave");
        r.id = 1;
        r.title = "T".repeat(200);
        r.snippet = "inline".into();
        r.snippet_full = "distinctive fulltext keyword appears here".into();
        let index = build_index(&[r]);
        let entry = &index[0];
        assert_eq!(entry["title"].as_str().unwrap().chars().count(), MANIFEST_TITLE_MAX_CHARS);
        let kws: Vec<String> = entry["keywords"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap().to_string())
            .collect();
        assert!(kws.contains(&"distinctive".to_string()), "keywords must come from snippet_full: {kws:?}");
    }

    #[test]
    fn inline_view_never_leaks_snippet_full() {
        let mut r = item("https://e.com", "brave");
        r.snippet_full = "SECRET-UNCAPPED-TEXT".into();
        let v = inline_view(&[r]);
        let serialized = serde_json::to_string(&v).unwrap();
        assert!(!serialized.contains("SECRET-UNCAPPED-TEXT"));
        assert!(!serialized.contains("snippet_full"));
    }

    #[test]
    fn read_chunk_rejects_path_traversal_in_search_id() {
        for bad in ["../etc/passwd", "a/b", "a\\b", ".."] {
            let out = web_search_read_chunk(bad, &[1]);
            let v: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(v["error_code"], "SEARCH_ID_NOT_FOUND", "must reject {bad}");
        }
    }

    #[test]
    fn read_chunk_reports_missing_search_id_cleanly() {
        let out = web_search_read_chunk("deadbeef-0000", &[1]);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "error");
        assert_eq!(v["error_code"], "SEARCH_ID_NOT_FOUND");
    }

    #[tokio::test]
    async fn empty_query_is_rejected_before_any_network_call() {
        for q in ["", "   ", "\t\n"] {
            let out = web_search(q, 5, "en", None, "US").await;
            let v: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(v["error_code"], "EMPTY_QUERY");
        }
    }

    /// End-to-end round trip through the real offload store, exercising the
    /// write → read_chunk path (and the `snippet_full` promotion) without any
    /// network. Uses a distinctive search_id so it can't collide with a real
    /// search's file.
    #[test]
    fn offload_write_then_read_chunk_round_trips_full_snippets() {
        let search_id = format!("test-{}", new_search_id());
        let results = vec![
            SearchItem {
                id: 1,
                title: "First".into(),
                domain: "e.com".into(),
                url: "https://e.com/1".into(),
                date: Some("2025-01-01".into()),
                snippet: "short".into(),
                snippet_full: "the full uncapped body text".into(),
                engine: "brave".into(),
            },
            SearchItem {
                id: 2,
                title: "Second".into(),
                domain: "f.com".into(),
                url: "https://f.com/2".into(),
                date: None,
                snippet: "short2".into(),
                snippet_full: "second full body".into(),
                engine: "duckduckgo".into(),
            },
        ];
        write_offload(&search_id, "q", &results);

        let out = web_search_read_chunk(&search_id, &[1, 2]);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "success");
        let data = v["data"].as_array().unwrap();
        assert_eq!(data.len(), 2);
        // read_chunk promotes snippet_full into `snippet`.
        assert_eq!(data[0]["snippet"], "the full uncapped body text");
        assert_eq!(data[1]["snippet"], "second full body");
        assert!(data[0].get("snippet_full").is_none());

        // Unknown ids in the same call are simply skipped.
        let partial = web_search_read_chunk(&search_id, &[2, 999]);
        let pv: Value = serde_json::from_str(&partial).unwrap();
        assert_eq!(pv["data"].as_array().unwrap().len(), 1);

        // All-unknown ids is a distinct, actionable error.
        let none = web_search_read_chunk(&search_id, &[999]);
        let nv: Value = serde_json::from_str(&none).unwrap();
        assert_eq!(nv["error_code"], "ID_NOT_FOUND");

        let _ = std::fs::remove_file(offload_path(&search_id));
    }

    #[test]
    fn read_chunk_caps_ids_at_five_and_flags_it() {
        let search_id = format!("test-{}", new_search_id());
        let results: Vec<SearchItem> = (1..=8)
            .map(|i| SearchItem {
                id: i,
                title: format!("T{i}"),
                domain: "e.com".into(),
                url: format!("https://e.com/{i}"),
                date: None,
                snippet: "s".into(),
                snippet_full: "f".into(),
                engine: "brave".into(),
            })
            .collect();
        write_offload(&search_id, "q", &results);

        let out = web_search_read_chunk(&search_id, &[1, 2, 3, 4, 5, 6, 7, 8]);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["data"].as_array().unwrap().len(), READ_CHUNK_MAX_IDS);
        assert_eq!(v["metadata"]["ids_truncated_to"], json!(READ_CHUNK_MAX_IDS));

        let _ = std::fs::remove_file(offload_path(&search_id));
    }
}
