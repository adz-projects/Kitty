//! rmcp server exposing the three web tools.
//!
//! Tool names are byte-identical to `kitty_docs_web.py`'s registrations —
//! adaptive-pathway keys learned routing preferences on the literal name
//! string, so renaming any of them orphans that history (see
//! `docs/PLUGINS.md`). Tool *descriptions* are likewise carried over close to
//! verbatim: they're the model-visible contract, and rewording them changes
//! tool-selection behavior for no benefit.

use std::panic::{catch_unwind, AssertUnwindSafe};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::envelope::error_response;
use crate::{scrape, search};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WebSearchRequest {
    /// The search query.
    pub query: String,
    /// Number of results. <=5 uses Brave-with-DuckDuckGo-fallback; 6-10
    /// queries both engines; >10 additionally returns a keyword index
    /// instead of full detail.
    pub count: Option<u32>,
    /// Two-letter search language code, e.g. "en".
    pub search_lang: Option<String>,
    /// Brave freshness filter, e.g. "pd", "pw", "pm", "py".
    pub freshness: Option<String>,
    /// Two-letter country code, e.g. "US".
    pub country: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WebSearchReadChunkRequest {
    /// The search_id returned by a prior lean_web_search call.
    pub search_id: String,
    /// Result ids to expand. At most 5 are honored per call.
    pub ids: Vec<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WebScrapeRequest {
    /// The URL to scrape.
    pub url: String,
    /// Optional keyword filter — returns only the markdown blocks matching it.
    pub query: Option<String>,
    /// "markdown" (default) or "text".
    pub output_format: Option<String>,
    /// Markdown-block offset to start from, for paging long pages.
    pub offset: Option<u32>,
    /// Character cap on the returned body. Defaults to 12000.
    pub max_chars: Option<u32>,
    /// Keep [label](url) links in the output instead of flattening to label.
    pub include_links: Option<bool>,
    /// Favor precision over recall in body extraction. Default favors recall.
    pub favor_precision: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct KittyWebServer {
    tool_router: ToolRouter<Self>,
}

impl Default for KittyWebServer {
    fn default() -> Self {
        Self::new()
    }
}

impl KittyWebServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::web_tool_router(),
        }
    }

    /// Sorted list of every registered tool name — used by `tests/protocol.rs`
    /// to pin the exact tool surface. Renaming any entry here orphans
    /// adaptive-pathway's learned routing for that tool, so this list must
    /// never be "tidied."
    pub fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        names.sort();
        names
    }
}

/// Runs `f`, converting a panic into the same `INTERNAL_PANIC` structured
/// error every other failure path returns — a malformed/adversarial input
/// must not kill the whole server process (and every subsequent call in the
/// session) the way `panic = "abort"` would. Mirrors kitty-tools' `guarded`.
fn guarded(f: impl FnOnce() -> String) -> String {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(s) => s,
        Err(_) => error_response(
            "INTERNAL_PANIC",
            "An internal error occurred while processing this request.",
            None,
            Some(
                "Retry with a different input; if this persists, the input may be malformed \
                 in a way this tool doesn't yet handle.",
            ),
        ),
    }
}

#[tool_router(router = web_tool_router)]
impl KittyWebServer {
    #[tool(
        name = "lean_web_search",
        description = "Searches the web. count<=5 (default): Brave if configured, DuckDuckGo only as a fallback on Brave failure. count 6-10: queries Brave AND DuckDuckGo together for broader coverage, still returned inline. count>10: same broadened fetch, but the full result set is offloaded to disk and a compact keyword index is returned instead of full detail. Every call returns a search_id; a large inline reply may be auto-downgraded to the keyword index (see \"downgraded_to_index\" in the response metadata). Follow up with lean_web_search_read_chunk for full detail on any id."
    )]
    pub async fn web_search(&self, Parameters(req): Parameters<WebSearchRequest>) -> String {
        search::web_search(
            &req.query,
            req.count.unwrap_or(5) as usize,
            req.search_lang.as_deref().unwrap_or("en"),
            req.freshness.as_deref(),
            req.country.as_deref().unwrap_or("US"),
        )
        .await
    }

    #[tool(
        name = "lean_web_search_read_chunk",
        description = "Fetches full url/snippet/date detail for specific result ids from a prior lean_web_search call — every search returns a search_id, not just the keyword-index ones."
    )]
    pub fn web_search_read_chunk(
        &self,
        Parameters(req): Parameters<WebSearchReadChunkRequest>,
    ) -> String {
        guarded(move || search::web_search_read_chunk(&req.search_id, &req.ids))
    }

    #[tool(
        name = "lean_web_scrape",
        description = "Scrapes a URL into clean Markdown (or plain text) for an LLM to read. offset and the response's metadata.next_offset page through long pages by markdown block, without severing a table or heading mid-cut. A PDF URL is downloaded to the cache and its local path is returned — use lean_pdf_read_text or lean_pdf_read_outline on that path next. favor_precision=True favors precision over recall; the default favors recall, since documentation/API-reference pages often lose sidebars and short definition blocks under the precision-favoring mode."
    )]
    pub async fn web_scrape(&self, Parameters(req): Parameters<WebScrapeRequest>) -> String {
        scrape::web_scrape(
            &req.url,
            req.query.as_deref(),
            req.output_format.as_deref().unwrap_or("markdown"),
            req.offset.unwrap_or(0) as usize,
            req.max_chars.map(|c| c as usize),
            req.include_links.unwrap_or(false),
            req.favor_precision.unwrap_or(false),
        )
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for KittyWebServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_surface_is_exactly_the_three_web_tools() {
        // Pinned deliberately: these names are what adaptive-pathway's
        // learned routing is keyed on, and what `kitty_docs_web.py`
        // registered. Adding a tool here is fine; renaming one is not.
        assert_eq!(
            KittyWebServer::new().tool_names(),
            vec![
                "lean_web_scrape".to_string(),
                "lean_web_search".to_string(),
                "lean_web_search_read_chunk".to_string(),
            ]
        );
    }

    #[test]
    fn guarded_converts_a_panic_into_a_structured_error() {
        let out = guarded(|| panic!("boom"));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "error");
        assert_eq!(v["error_code"], "INTERNAL_PANIC");
    }
}
