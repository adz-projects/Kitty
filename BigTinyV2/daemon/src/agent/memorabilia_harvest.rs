//! Turn-end document harvest for the memorabilia factual-memory plugin.
//!
//! Replaces the old "ingest the latest user+assistant pair" behavior. Message
//! pairs are weak evidence — user and model are both frequently wrong — so
//! instead we ingest the *documents* a turn actually brought in:
//!   1. **Pasted text** and inlined text-file attachments — the
//!      `--- <label> ---\n<body>` blocks the client prepends to the user
//!      message (see `chatStore.ts`'s inlined-attachment prompt building).
//!   2. **Files attached by path** — the `Files provided by the user:` block;
//!      each file's text is extracted with `kitty_tools::extract`
//!      (reusing the same PDF/Word/Excel/text extractors the MCP tools use).
//!   3. **Successfully scraped pages** — harvested from this turn's persisted
//!      `lean_web_scrape` tool results (no re-scrape). A scrape that downloaded
//!      a document (pdf/docx/…) is extracted from its `cached_path`.
//!
//! Everything is scoped to the just-completed turn (this session's latest user
//! message and the rows after it) and is idempotent: memorabilia dedups by
//! document hash, so re-running a turn writes nothing new. Soft-fail
//! throughout — a bad file or envelope is logged and skipped.

use std::collections::HashSet;
use std::path::PathBuf;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use sqlx::SqlitePool;

use memorabilia::engine::Engine;
use memorabilia::learn::{AttachmentIntent, IngestInput};

/// The kitty-web scrape tool name, matched against assistant `tool_calls` to
/// find scraped pages among this turn's tool results.
const SCRAPE_TOOL: &str = "lean_web_scrape";

/// The client's inlined-attachment path-list header (see `chatStore.ts`).
const FILES_HEADER: &str = "Files provided by the user:";

/// Harvest and ingest this turn's documents. Called fire-and-forget from the
/// agent loop's turn-end hook, after the pause check.
pub async fn harvest_turn(engine: &Engine, pool: &SqlitePool, session_id: &str) {
    let Some((user_rowid, user_content)) = latest_user_message(pool, session_id).await else {
        return;
    };
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut ingested_any = false;

    // Source 1 — pasted text & inlined documents (already text). Parse only the
    // region before the `Files provided by the user:` list, so a paths block or
    // trailing typed text can't be swallowed into the last marker body.
    for (label, body) in parse_marker_blocks(docs_region(&user_content)) {
        let is_paste = label.starts_with("Pasted text");
        let (source_type, source_entity) = if is_paste {
            ("UserNote", "user:paste".to_string())
        } else {
            ("Attachment", format!("inlined:{label}"))
        };
        ingest(
            engine,
            &now,
            source_type,
            &label,
            &source_entity,
            Some(AttachmentIntent::Evidence),
            body,
        )
        .await;
        ingested_any = true;
    }

    // Source 2 — attached files by path; extract text via kitty-tools.
    for path in parse_file_paths(&user_content) {
        let p = PathBuf::from(&path);
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path.as_str())
            .to_string();
        match extract_file(p).await {
            Some(text) => {
                ingest(
                    engine,
                    &now,
                    "Attachment",
                    &name,
                    &path,
                    Some(AttachmentIntent::Evidence),
                    text,
                )
                .await;
                ingested_any = true;
            }
            None => {}
        }
    }

    // Source 3 — successfully scraped pages from this turn's tool results.
    for item in scraped_items(pool, session_id, user_rowid).await {
        let content = match item.body {
            ScrapeBody::Text(t) => Some(t),
            ScrapeBody::CachedDoc(path) => extract_file(PathBuf::from(path)).await,
        };
        if let Some(content) = content {
            // Web scrapes carry no attachment intent; memorabilia tiers them by
            // domain (`source_type == "Scraped"` + `source_name == host`).
            ingest(engine, &now, "Scraped", &item.host, &item.url, None, content).await;
            ingested_any = true;
        }
    }

    // Make what we just ingested recallable without waiting for the sweep.
    if ingested_any {
        engine.drain_extraction(&now).await;
    }
}

/// Ingest one document, skipping empties. Errors are swallowed by the engine
/// (soft-fail); the returned outcome is not needed here.
async fn ingest(
    engine: &Engine,
    now: &str,
    source_type: &str,
    source_name: &str,
    source_entity: &str,
    intent: Option<AttachmentIntent>,
    content: String,
) {
    if content.trim().is_empty() {
        return;
    }
    engine
        .ingest(&IngestInput {
            content,
            source_type: source_type.to_string(),
            source_name: source_name.to_string(),
            source_entity: source_entity.to_string(),
            captured_at: now.to_string(),
            intent,
        })
        .await;
}

/// Extract a file's text on the blocking pool (PDF/Excel parsing is CPU-bound).
/// `None` on an unsupported type, empty text, or any error — all logged+skipped.
async fn extract_file(path: PathBuf) -> Option<String> {
    let path_str = path.display().to_string();
    let result = tokio::task::spawn_blocking(move || {
        kitty_tools::extract::extract_document_text(&path)
    })
    .await;
    match result {
        Ok(Ok(text)) if !text.trim().is_empty() => Some(text),
        Ok(Ok(_)) => None,
        Ok(Err(e)) => {
            tracing::debug!("memorabilia harvest: skipped {path_str}: {e}");
            None
        }
        Err(e) => {
            tracing::warn!("memorabilia harvest: extraction task failed for {path_str}: {e}");
            None
        }
    }
}

async fn latest_user_message(pool: &SqlitePool, session_id: &str) -> Option<(i64, String)> {
    sqlx::query_as::<_, (i64, String)>(
        "SELECT rowid, COALESCE(content, '') FROM messages \
         WHERE session_id = ? AND role = 'user' ORDER BY rowid DESC LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// The message region before the `Files provided by the user:` list (or the
/// whole message when there is none) — where the `--- label ---` blocks live.
fn docs_region(content: &str) -> &str {
    match content.find(FILES_HEADER) {
        Some(idx) => &content[..idx],
        None => content,
    }
}

/// Split leading `--- <label> ---\n<body>` blocks. Each marker line starts a
/// block whose body runs until the next marker (or the end of `region`). When
/// no file-path list is present, the final block may include the user's
/// trailing typed text — accepted: capturing the whole pasted document matters
/// more than excluding a short trailing prompt, and the PII gate + dedup still
/// apply.
fn parse_marker_blocks(region: &str) -> Vec<(String, String)> {
    static MARKER: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^--- (.+) ---$").unwrap());
    let mut blocks: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, Vec<&str>)> = None;
    for line in region.lines() {
        if let Some(cap) = MARKER.captures(line.trim_end()) {
            if let Some((label, body)) = current.take() {
                blocks.push((label, body.join("\n")));
            }
            current = Some((cap[1].trim().to_string(), Vec::new()));
        } else if let Some((_, body)) = current.as_mut() {
            body.push(line);
        }
        // Lines before the first marker are ignored.
    }
    if let Some((label, body)) = current.take() {
        blocks.push((label, body.join("\n")));
    }
    blocks
        .into_iter()
        .map(|(l, b)| (l, b.trim().to_string()))
        .filter(|(_, b)| !b.is_empty())
        .collect()
}

/// Absolute paths from the `Files provided by the user:\n- <path>` block.
fn parse_file_paths(content: &str) -> Vec<String> {
    let Some(idx) = content.find(FILES_HEADER) else {
        return Vec::new();
    };
    let region = &content[idx + FILES_HEADER.len()..];
    let mut paths = Vec::new();
    let mut started = false;
    for line in region.lines() {
        let t = line.trim();
        if let Some(p) = t.strip_prefix("- ") {
            started = true;
            let p = p.trim();
            if !p.is_empty() {
                paths.push(p.to_string());
            }
        } else if started {
            break; // list ended (typed text follows)
        } else if t.is_empty() {
            continue; // the newline right after the header
        } else {
            break;
        }
    }
    paths
}

enum ScrapeBody {
    /// An HTML page scrape — the extracted markdown/text.
    Text(String),
    /// A scrape that downloaded a document; extract from this cached path.
    CachedDoc(String),
}

struct ScrapedItem {
    url: String,
    host: String,
    body: ScrapeBody,
}

/// Successful `lean_web_scrape` results from this turn (rows after the user
/// message). The tool name is only on the assistant row's `tool_calls`, so we
/// first collect the scrape `tool_call_id`s, then match the `role='tool'` rows.
async fn scraped_items(pool: &SqlitePool, session_id: &str, user_rowid: i64) -> Vec<ScrapedItem> {
    let assistant_rows: Vec<(String,)> = sqlx::query_as(
        "SELECT tool_calls FROM messages \
         WHERE session_id = ? AND rowid > ? AND role = 'assistant' AND tool_calls IS NOT NULL",
    )
    .bind(session_id)
    .bind(user_rowid)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let mut scrape_ids: HashSet<String> = HashSet::new();
    for (tc_json,) in assistant_rows {
        collect_scrape_ids(&tc_json, &mut scrape_ids);
    }
    if scrape_ids.is_empty() {
        return Vec::new();
    }

    let tool_rows: Vec<(Option<String>, String)> = sqlx::query_as(
        "SELECT tool_call_id, COALESCE(content, '') FROM messages \
         WHERE session_id = ? AND rowid > ? AND role = 'tool'",
    )
    .bind(session_id)
    .bind(user_rowid)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let mut out = Vec::new();
    for (tcid, content) in tool_rows {
        match tcid {
            Some(id) if scrape_ids.contains(&id) => {
                if let Some(item) = parse_scrape_success(&content) {
                    out.push(item);
                }
            }
            _ => {}
        }
    }
    out
}

/// Add the `tool_call_id`s of any `lean_web_scrape` calls in one assistant
/// row's `tool_calls` JSON to `ids`.
fn collect_scrape_ids(tool_calls_json: &str, ids: &mut HashSet<String>) {
    let Ok(Value::Array(calls)) = serde_json::from_str::<Value>(tool_calls_json) else {
        return;
    };
    for call in calls {
        let name = call
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str());
        let id = call.get("id").and_then(|v| v.as_str());
        if let (Some(id), Some(name)) = (id, name) {
            if name == SCRAPE_TOOL {
                ids.insert(id.to_string());
            }
        }
    }
}

/// Parse a `lean_web_scrape` envelope; `Some` only for `status == "success"`
/// with usable content. `data` is a string for an HTML page, or an object with
/// `cached_path` for a downloaded document.
fn parse_scrape_success(envelope: &str) -> Option<ScrapedItem> {
    let v: Value = serde_json::from_str(envelope).ok()?;
    if v.get("status").and_then(|s| s.as_str()) != Some("success") {
        return None;
    }
    let data = v.get("data")?;
    let url = v
        .get("metadata")
        .and_then(|m| m.get("final_url").or_else(|| m.get("url")))
        .and_then(|u| u.as_str())
        .map(str::to_string);

    if let Some(text) = data.as_str() {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let url = url.unwrap_or_default();
        let host = host_of(&url);
        Some(ScrapedItem {
            host,
            url,
            body: ScrapeBody::Text(text.to_string()),
        })
    } else if let Some(cached) = data.get("cached_path").and_then(|p| p.as_str()) {
        // A downloaded document: URL lives on `data.url` for this shape.
        let url = url
            .or_else(|| data.get("url").and_then(|u| u.as_str()).map(str::to_string))
            .unwrap_or_default();
        let host = host_of(&url);
        Some(ScrapedItem {
            host,
            url,
            body: ScrapeBody::CachedDoc(cached.to_string()),
        })
    } else {
        None
    }
}

/// Bare host of a URL (for the `Scraped` source's domain tiering), no scheme,
/// userinfo, port, path, or query. Falls back to the whole string.
fn host_of(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let host = authority.rsplit('@').next().unwrap_or(authority);
    host.split(':').next().unwrap_or(host).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_blocks_capture_paste_and_inlined_file() {
        let msg = "--- Pasted text — 3 words ---\nalpha beta gamma\n\n\
                   --- notes.txt ---\nfile body line one\nfile body line two";
        let blocks = parse_marker_blocks(docs_region(msg));
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, "Pasted text — 3 words");
        assert_eq!(blocks[0].1, "alpha beta gamma");
        assert_eq!(blocks[1].0, "notes.txt");
        assert_eq!(blocks[1].1, "file body line one\nfile body line two");
    }

    #[test]
    fn docs_region_excludes_the_files_list_and_typed_text() {
        let msg = "--- doc.md ---\nreal document content\n\n\
                   Files provided by the user:\n- C:\\a\\report.pdf\n\nSummarize these.";
        let blocks = parse_marker_blocks(docs_region(msg));
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].1, "real document content");
        let paths = parse_file_paths(msg);
        assert_eq!(paths, vec!["C:\\a\\report.pdf".to_string()]);
    }

    #[test]
    fn plain_message_yields_no_documents() {
        let msg = "just a normal question with no attachments";
        assert!(parse_marker_blocks(docs_region(msg)).is_empty());
        assert!(parse_file_paths(msg).is_empty());
    }

    #[test]
    fn scrape_success_string_is_parsed_error_is_skipped() {
        let ok = r#"{"status":"success","data":"Title body","metadata":{"final_url":"https://sub.example.com:443/p?x=1","title":"T"}}"#;
        let item = parse_scrape_success(ok).expect("success parses");
        assert_eq!(item.host, "sub.example.com");
        assert_eq!(item.url, "https://sub.example.com:443/p?x=1");
        assert!(matches!(item.body, ScrapeBody::Text(_)));

        let err = r#"{"status":"error","error_code":"SCRAPE_EMPTY","message":"no content"}"#;
        assert!(parse_scrape_success(err).is_none());
    }

    #[test]
    fn scrape_downloaded_document_becomes_a_cached_doc() {
        let dl = r#"{"status":"success","data":{"cached_path":"/c/cache/doc.pdf","url":"https://example.org/f.pdf","file_type":"pdf"}}"#;
        let item = parse_scrape_success(dl).expect("download parses");
        assert_eq!(item.host, "example.org");
        match item.body {
            ScrapeBody::CachedDoc(p) => assert_eq!(p, "/c/cache/doc.pdf"),
            _ => panic!("expected CachedDoc"),
        }
    }

    #[test]
    fn collect_scrape_ids_matches_only_the_scrape_tool() {
        let json = r#"[
            {"id":"call_1","type":"function","function":{"name":"lean_web_scrape","arguments":"{}"}},
            {"id":"call_2","type":"function","function":{"name":"lean_web_search","arguments":"{}"}}
        ]"#;
        let mut ids = HashSet::new();
        collect_scrape_ids(json, &mut ids);
        assert!(ids.contains("call_1"));
        assert!(!ids.contains("call_2"));
        assert_eq!(ids.len(), 1);
    }
}
