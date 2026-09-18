//! In-process MCP lookup server (BigTiny-plugin conversion): the on-demand
//! read path that complements the per-turn context injection. The plugin
//! injects a compact summary + index of relevant items into the model's
//! context; when the model wants the full text of an item it calls one of
//! these tools with the `item_id` from the index — exactly the kitty-tools
//! "index → look it up" loop (`document_id → doc_read_chunk / doc_search`),
//! here `item_id → memorabilia_read_item / memorabilia_search`.
//!
//! Conventions mirror kitty-tools: a uniform `{status, truncated, data,
//! message?, metadata?}` envelope, `SCREAMING_SNAKE` domain-prefixed error
//! codes, zero-based `offset`/`next_offset`/`has_more` pagination, and
//! load-bearing tool names. Tools are read-only — grounding/reinforcement is
//! written only by the per-turn injection (`Engine::recall`), so a lookup
//! never double-counts a query event. The server holds the per-app
//! `Arc<Engine>`, so it reads the plugin's own store in-process; the same
//! rmcp major as kitty-tools/adaptive-pathway keeps the in-process transport
//! wiring identical.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::engine::Engine;

/// Default page size for `memorabilia_search` (kitty-tools uses 200 for raw
/// document units; a ranked memory index is denser, so a smaller default
/// keeps the first page decision-sized).
const SEARCH_DEFAULT_LIMIT: usize = 25;
/// Hard ceiling on a search page, so a caller cannot request an unbounded scan.
const SEARCH_MAX_LIMIT: usize = 200;
/// Default supporting-chunk window for `memorabilia_read_item`.
const READ_DEFAULT_LIMIT: usize = 10;
/// Hard ceiling on one read window.
const READ_MAX_LIMIT: usize = 100;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchRequest {
    /// Natural-language query to match against stored memory. Returns ranked
    /// items with their item_id, claim, confidence, and citation.
    pub query: String,
    /// Zero-based offset into the ranked results, for pagination. Default 0.
    pub offset: Option<u32>,
    /// Maximum items to return. Default 25, capped at 200.
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadItemRequest {
    /// The item_id from a memorabilia_search result or an injected index line
    /// (never a guessed value). Resolves to the item's full claim plus its
    /// supporting evidence and citations.
    pub item_id: String,
    /// Zero-based offset into the item's supporting chunks, for pagination.
    /// Default 0.
    pub offset: Option<u32>,
    /// Maximum supporting chunks to return. Default 10, capped at 100.
    pub limit: Option<u32>,
}

fn success(data: Value, truncated: bool, metadata: Value) -> String {
    json!({
        "status": "success",
        "truncated": truncated,
        "data": data,
        "metadata": metadata,
    })
    .to_string()
}

fn success_msg(data: Value, message: &str) -> String {
    json!({
        "status": "success",
        "truncated": false,
        "data": data,
        "message": message,
    })
    .to_string()
}

fn error(error_code: &str, message: &str, hint: &str) -> String {
    json!({
        "status": "error",
        "error_code": error_code,
        "message": message,
        "hint": hint,
    })
    .to_string()
}

#[derive(Clone)]
pub struct MemorabiliaServer {
    engine: Arc<Engine>,
    tool_router: ToolRouter<Self>,
}

impl MemorabiliaServer {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            tool_router: Self::core_tool_router(),
            engine,
        }
    }

    /// Sorted list of every registered tool name (mirrors the pathway server;
    /// used by the daemon's advertised-vs-connected conformance check).
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

    /// Serve the in-process MCP server over an arbitrary duplex stream
    /// (mirrors `PathwayServer::serve_in_process`).
    pub async fn serve_in_process<S>(&self, stream: S) -> Result<(), String>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
    {
        let server = self.clone().serve(stream).await.map_err(|e| e.to_string())?;
        server.waiting().await.map(|_| ()).map_err(|e| e.to_string())
    }
}

#[tool_router(router = core_tool_router)]
impl MemorabiliaServer {
    /// Search the app's active memory for items relevant to a query.
    #[tool(
        name = "memorabilia_search",
        description = "Search stored memory for items relevant to a query. Returns ranked items with their item_id, claim, confidence, and citation. Use the item_id with memorabilia_read_item to read an item's full supporting evidence. Supports offset-based pagination."
    )]
    pub async fn memorabilia_search(&self, Parameters(req): Parameters<SearchRequest>) -> String {
        let query = req.query.trim();
        if query.is_empty() {
            return success_msg(json!([]), "empty query; nothing searched");
        }
        let offset = req.offset.unwrap_or(0) as usize;
        let limit = (req.limit.unwrap_or(SEARCH_DEFAULT_LIMIT as u32) as usize)
            .clamp(1, SEARCH_MAX_LIMIT);
        // Over-fetch one to detect a further page.
        let entries = self.engine.search_index(query, offset, limit + 1).await;
        if entries.is_empty() {
            return success_msg(json!([]), "no relevant memory found");
        }
        let has_more = entries.len() > limit;
        let page = &entries[..entries.len().min(limit)];
        let data: Vec<Value> = page
            .iter()
            .map(|e| {
                json!({
                    "item_id": e.item_id,
                    "claim": e.claim,
                    "confidence": e.confidence,
                    "disputed": e.is_disputed,
                    "citation": e.citation,
                })
            })
            .collect();
        let mut metadata = json!({
            "offset": offset,
            "returned": page.len(),
            "has_more": has_more,
        });
        if has_more {
            metadata["next_offset"] = json!(offset + limit);
        }
        success(json!(data), has_more, metadata)
    }

    /// Read one item's full claim, supporting evidence, and citations.
    #[tool(
        name = "memorabilia_read_item",
        description = "Read one memory item in full by its item_id (from memorabilia_search or an injected memory index): its claim, confidence, importance/urgency, whether it is disputed, and its supporting evidence with citations. Supports offset-based pagination over the supporting evidence."
    )]
    pub async fn memorabilia_read_item(
        &self,
        Parameters(req): Parameters<ReadItemRequest>,
    ) -> String {
        let item_id = req.item_id.trim();
        if item_id.is_empty() {
            return error(
                "MEMORABILIA_BAD_REQUEST",
                "item_id is required",
                "Pass an item_id from a memorabilia_search result or an injected index line.",
            );
        }
        let offset = req.offset.unwrap_or(0) as usize;
        let limit = (req.limit.unwrap_or(READ_DEFAULT_LIMIT as u32) as usize)
            .clamp(1, READ_MAX_LIMIT);
        let Some(view) = self.engine.render_item(item_id, offset, limit).await else {
            return error(
                "MEMORABILIA_ITEM_NOT_FOUND",
                &format!("no active memory item with id '{item_id}'"),
                "Item ids change as memory updates; call memorabilia_search again to get a current id.",
            );
        };
        let chunks: Vec<Value> = view
            .chunks
            .iter()
            .map(|c| json!({ "citation": c.citation, "content": c.content }))
            .collect();
        let returned = view.chunks.len();
        let next = view.offset + returned;
        let has_more = next < view.total_chunks;
        let data = json!({
            "item_id": view.item_id,
            "claim": view.claim,
            "confidence": view.confidence,
            "importance": view.importance,
            "urgency": view.urgency,
            "disputed": view.is_disputed,
            "chunks": chunks,
        });
        let mut metadata = json!({
            "offset": view.offset,
            "total_chunks": view.total_chunks,
            "returned": returned,
            "has_more": has_more,
        });
        if has_more {
            metadata["next_offset"] = json!(next);
        }
        success(data, has_more, metadata)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MemorabiliaServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
}

/// Serve `MemorabiliaServer` over a duplex stream, for the BigTiny daemon's
/// in-process transport wiring (mirrors `adaptive_pathway::mcp::serve_in_process`).
pub async fn serve_in_process<S>(server: Arc<MemorabiliaServer>, stream: S) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    server.serve_in_process(stream).await
}
