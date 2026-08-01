use std::panic::{catch_unwind, AssertUnwindSafe};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::docx;
use crate::docx::write::WriteMode;
use crate::envelope::{error_response, success_response};
use crate::paths::resolve;
use crate::query_filter::filter_by_query;
use crate::tools;
use crate::tools::viz::VizStep;

/// Paragraphs returned per page when no `limit` is given — same default as
/// `lean_file_read`'s `file_page_size` threshold in the Python plugin this
/// replaces the Word tools of.
const DEFAULT_PAGE_SIZE: u32 = 200;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WordReadTextRequest {
    /// Path to the .docx file.
    pub path: String,
    /// Optional keyword filter over paragraphs.
    pub query: Option<String>,
    /// Zero-based paragraph index to start from (pagination / query
    /// continuation — see `metadata.next_offset` on a truncated response).
    pub offset: Option<u32>,
    /// Max paragraphs to return when no query is given (default 200).
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WordReadOutlineRequest {
    /// Path to the .docx file.
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WordWriteModeParam {
    Create,
    Append,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WordWriteDocRequest {
    /// Path to the .docx file to create or append to.
    pub path: String,
    /// Markdown-lite body text (headings, lists, tables, **bold**/*italic*).
    pub doc_text: Option<String>,
    /// "create" (default) or "append".
    pub write_mode: Option<WordWriteModeParam>,
    /// Document title (create mode defaults to the file stem; append mode
    /// leaves the existing title untouched unless given).
    pub title: Option<String>,
    /// BCP-47 language tag for the WCAG `w:lang` metadata (default "en-US").
    pub language: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShellRequest {
    /// The shell command to run (executed via `cmd /c`).
    pub command: String,
    /// Preview without executing.
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AnalyzeWorkspaceRequest {
    /// Directory (or file) to inspect. Defaults to ".".
    pub path: Option<String>,
    /// Max recursion depth. Defaults to 10.
    pub max_depth: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileReadRequest {
    pub path: String,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub query: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileWriteRequest {
    pub path: String,
    pub content: String,
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileAppendRequest {
    pub path: String,
    pub content: String,
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileReplaceStrRequest {
    pub path: String,
    pub old_str: String,
    pub new_str: String,
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileReplaceLinesRequest {
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
    pub new_content: String,
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CacheFilenameRequest {
    pub filename: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScratchpadSetRequest {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScratchpadKeyRequest {
    pub key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AccessibleTableRequest {
    pub title: String,
    pub headers: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub summary: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VizStepParam {
    pub text: Option<String>,
    #[serde(rename = "type")]
    pub step_type: Option<String>,
    pub subtitle: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AccessibleSvgRequest {
    /// One of "flowchart", "single_lane", "swimlane", or "journey_map".
    pub diagram_type: String,
    pub title: String,
    pub description: String,
    pub steps: Option<Vec<VizStepParam>>,
}

#[derive(Debug, Clone)]
pub struct KittyToolsServer {
    tool_router: ToolRouter<Self>,
}

impl Default for KittyToolsServer {
    fn default() -> Self {
        Self::new()
    }
}

impl KittyToolsServer {
    /// Assembles the router from two pieces: the 18 always-on `lean_*`
    /// local-machine tools, plus the 2 visualization tools, included only
    /// when `KITTY_VIZ_ENABLED=1`. Web search (`lean_web_search` /
    /// `lean_web_search_read_chunk`) lives in the Python `kitty-docs-web`
    /// process instead — see `docs/VERSIONS.md` for why the merged
    /// Brave/DuckDuckGo search tool moved out of this crate. Per the base
    /// plan: "remove tools from the router at startup rather than
    /// registering them and failing at call time" — env is fixed for the
    /// process lifetime and BigTiny restarts this server whenever its spec
    /// (and therefore its env) changes, so a disabled tool is simply never
    /// advertised rather than advertised-then-erroring, which would burn
    /// context and invite the model to call something guaranteed to fail.
    pub fn new() -> Self {
        let mut router = Self::core_tool_router();
        if std::env::var("KITTY_VIZ_ENABLED").as_deref() == Ok("1") {
            router += Self::viz_tool_router();
        }
        Self { tool_router: router }
    }

    /// Sorted list of every currently-registered tool name — used by
    /// `tests/protocol.rs` to pin the exact tool surface. Renaming any entry
    /// here orphans adaptive-pathway's learned routing for that tool (see
    /// the base plan's "tool names are load-bearing" section), so this list
    /// must never be "tidied."
    pub fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tool_router.list_all().into_iter().map(|t| t.name.to_string()).collect();
        names.sort();
        names
    }
}

/// Runs `f`, converting a panic into the same `INTERNAL_PANIC` structured
/// error every other failure path returns — a malformed/adversarial input
/// must not kill the whole server process (and every subsequent call in the
/// session) the way `panic = "abort"` would.
fn guarded(f: impl FnOnce() -> String) -> String {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(s) => s,
        Err(_) => error_response(
            "INTERNAL_PANIC",
            "An internal error occurred while processing this request.",
            None,
            Some("Retry with a different input; if this persists, the input may be malformed in a way this tool doesn't yet handle."),
        ),
    }
}

#[tool_router(router = core_tool_router)]
impl KittyToolsServer {
    #[tool(name = "lean_word_read_text", description = "Reads body text from a Word .docx, reaching paragraphs inside tables and text boxes. Supports offset-based pagination and keyword query filtering.")]
    pub fn word_read_text(&self, Parameters(req): Parameters<WordReadTextRequest>) -> String {
        guarded(move || {
            let resolved = resolve(&req.path);
            let paragraphs = match docx::read_paragraphs(&resolved) {
                Ok(p) => p,
                Err(docx::DocxError::NotFound) => {
                    return error_response("DOCX_NOT_FOUND", "Document does not exist", Some(&resolved.to_string_lossy()), None);
                }
                Err(docx::DocxError::Corrupt(detail)) => {
                    return error_response("DOCX_CORRUPT", &format!("Cannot open docx: {detail}"), Some(&resolved.to_string_lossy()), None);
                }
            };
            let texts: Vec<String> = paragraphs.iter().map(|p| p.text.clone()).collect();
            let offset = req.offset.unwrap_or(0) as usize;

            if let Some(query) = req.query.as_deref().filter(|q| !q.trim().is_empty()) {
                let result = filter_by_query(&texts, Some(query), 50, offset);
                let message = result
                    .no_match
                    .then(|| format!("No direct matches for query '{query}'. Showing top section."));
                let mut metadata = json!({
                    "read_method": "xml_scan",
                    "filtered_by_query": query,
                    "total_matches": result.total_matches,
                    "offset": offset,
                });
                if let Some(next) = result.next_offset {
                    metadata["next_offset"] = json!(next);
                }
                return success_response(
                    json!(result.items),
                    message.as_deref(),
                    result.truncated,
                    Some(metadata),
                );
            }

            let limit = req.limit.unwrap_or(DEFAULT_PAGE_SIZE) as usize;
            let total = texts.len();
            let page: Vec<String> = texts.iter().skip(offset).take(limit).cloned().collect();
            let has_more = offset + page.len() < total;
            let mut metadata = json!({
                "read_method": "xml_scan",
                "offset": offset,
                "total_paragraphs": total,
                "has_more": has_more,
            });
            if has_more {
                metadata["next_offset"] = json!(offset + page.len());
            }
            success_response(json!(page), None, has_more, Some(metadata))
        })
    }

    #[tool(name = "lean_word_read_outline", description = "Returns the heading structure (levels 1-4) of a Word document, reaching headings inside tables and text boxes.")]
    pub fn word_read_outline(&self, Parameters(req): Parameters<WordReadOutlineRequest>) -> String {
        guarded(move || {
            let resolved = resolve(&req.path);
            let paragraphs = match docx::read_paragraphs(&resolved) {
                Ok(p) => p,
                Err(docx::DocxError::NotFound) => {
                    return error_response("DOCX_NOT_FOUND", "Document does not exist", Some(&resolved.to_string_lossy()), None);
                }
                Err(docx::DocxError::Corrupt(detail)) => {
                    return error_response("DOCX_CORRUPT", &format!("Cannot open docx: {detail}"), Some(&resolved.to_string_lossy()), None);
                }
            };
            let outline: Vec<_> = paragraphs
                .iter()
                .filter_map(|p| {
                    p.heading_level
                        .filter(|lvl| (1..=4).contains(lvl))
                        .map(|lvl| json!({"level": lvl, "text": p.text}))
                })
                .collect();
            success_response(json!(outline), None, false, Some(json!({"read_method": "xml_scan"})))
        })
    }

    #[tool(name = "lean_word_write_doc", description = "Writes a new Word document or appends to an existing one, from markdown-lite text (headings, lists, tables, bold/italic), with WCAG accessibility structures.")]
    pub fn word_write_doc(&self, Parameters(req): Parameters<WordWriteDocRequest>) -> String {
        guarded(move || {
            let resolved = resolve(&req.path);
            let mode = match req.write_mode {
                Some(WordWriteModeParam::Append) => WriteMode::Append,
                _ => WriteMode::Create,
            };
            let language = req.language.as_deref().unwrap_or("en-US");

            if matches!(mode, WriteMode::Append) && !resolved.exists() {
                return error_response("DOCX_NOT_FOUND", "Document does not exist", Some(&resolved.to_string_lossy()), None);
            }

            let mode_label = if matches!(mode, WriteMode::Append) { "append" } else { "create" };

            match docx::write::write_document(&resolved, req.doc_text.as_deref(), mode, req.title.as_deref(), language) {
                Ok(result) => success_response(
                    json!({"path": result.path, "mode": result.mode, "language": result.language}),
                    Some("Document saved with WCAG accessibility metadata."),
                    false,
                    None,
                ),
                Err(docx::DocxError::NotFound) => {
                    error_response("DOCX_NOT_FOUND", "Document does not exist", Some(&resolved.to_string_lossy()), None)
                }
                Err(docx::DocxError::Corrupt(detail)) => {
                    // A same-name file locked open in Word (PermissionError
                    // on the equivalent Python path) surfaces here as an I/O
                    // failure during the write; distinguish it so the model
                    // gets an actionable hint instead of a generic corrupt-
                    // file message for what is really a save-mode failure.
                    if detail.to_lowercase().contains("denied") || detail.to_lowercase().contains("used by another process") {
                        error_response(
                            "DOCX_LOCKED",
                            "Could not save the document — it may be open in Word.",
                            Some(&detail),
                            Some("Close the file in Word (or any other program with it open) and try again."),
                        )
                    } else {
                        error_response("DOCX_WRITE_ERROR", &format!("Cannot {mode_label} docx: {detail}"), Some(&resolved.to_string_lossy()), None)
                    }
                }
            }
        })
    }

    #[tool(name = "lean_shell", description = "Runs a shell command and returns truncated stdout/stderr. Set dry_run=True to preview without executing.")]
    pub async fn shell(&self, Parameters(req): Parameters<ShellRequest>) -> String {
        tools::shell::shell(&req.command, req.dry_run.unwrap_or(false)).await
    }

    #[tool(name = "lean_analyze_workspace", description = "Lists files and folders under path (or returns metadata if path is a file).")]
    pub fn analyze_workspace(&self, Parameters(req): Parameters<AnalyzeWorkspaceRequest>) -> String {
        guarded(move || tools::workspace::analyze_workspace(req.path.as_deref().unwrap_or("."), req.max_depth))
    }

    #[tool(name = "lean_file_read", description = "Reads lines from a text file with line numbers. Supports query filtering.")]
    pub fn file_read(&self, Parameters(req): Parameters<FileReadRequest>) -> String {
        guarded(move || tools::fs::file_read(&req.path, req.start_line, req.end_line, req.query.as_deref()))
    }

    #[tool(name = "lean_file_write", description = "Overwrites (or creates) a text file with the given content.")]
    pub fn file_write(&self, Parameters(req): Parameters<FileWriteRequest>) -> String {
        guarded(move || tools::fs::file_write(&req.path, &req.content, req.dry_run.unwrap_or(false)))
    }

    #[tool(name = "lean_file_append", description = "Appends content to the end of an existing text file.")]
    pub fn file_append(&self, Parameters(req): Parameters<FileAppendRequest>) -> String {
        guarded(move || tools::fs::file_append(&req.path, &req.content, req.dry_run.unwrap_or(false)))
    }

    #[tool(name = "lean_file_replace_str", description = "Replaces exact string occurrences in a file.")]
    pub fn file_replace_str(&self, Parameters(req): Parameters<FileReplaceStrRequest>) -> String {
        guarded(move || tools::fs::file_replace_str(&req.path, &req.old_str, &req.new_str, req.dry_run.unwrap_or(false)))
    }

    #[tool(name = "lean_file_replace_lines", description = "Replaces a specific 1-indexed inclusive line range with new content.")]
    pub fn file_replace_lines(&self, Parameters(req): Parameters<FileReplaceLinesRequest>) -> String {
        guarded(move || {
            tools::fs::file_replace_lines(&req.path, req.start_line, req.end_line, &req.new_content, req.dry_run.unwrap_or(false))
        })
    }

    #[tool(name = "lean_cache_list", description = "Lists files currently stored in the scratch cache directory with their sizes.")]
    pub fn cache_list(&self) -> String {
        guarded(tools::cache::cache_list)
    }

    #[tool(name = "lean_cache_view", description = "Reads the text content of a file previously stored in the scratch cache directory.")]
    pub fn cache_view(&self, Parameters(req): Parameters<CacheFilenameRequest>) -> String {
        guarded(move || tools::cache::cache_view(&req.filename))
    }

    #[tool(name = "lean_cache_delete", description = "Deletes a single file from the scratch cache directory.")]
    pub fn cache_delete(&self, Parameters(req): Parameters<CacheFilenameRequest>) -> String {
        guarded(move || tools::cache::cache_delete(&req.filename))
    }

    #[tool(name = "lean_cache_clear", description = "Deletes every file in the scratch cache directory and returns how many were removed.")]
    pub fn cache_clear(&self) -> String {
        guarded(tools::cache::cache_clear)
    }

    #[tool(name = "lean_scratchpad_set", description = "Stores a key/value pair in the persistent scratchpad for recall across turns.")]
    pub fn scratchpad_set(&self, Parameters(req): Parameters<ScratchpadSetRequest>) -> String {
        guarded(move || tools::scratchpad::scratchpad_set(&req.key, &req.value))
    }

    #[tool(name = "lean_scratchpad_get", description = "Retrieves a previously stored scratchpad value by key.")]
    pub fn scratchpad_get(&self, Parameters(req): Parameters<ScratchpadKeyRequest>) -> String {
        guarded(move || tools::scratchpad::scratchpad_get(&req.key))
    }

    #[tool(name = "lean_scratchpad_delete", description = "Deletes a key from the persistent scratchpad.")]
    pub fn scratchpad_delete(&self, Parameters(req): Parameters<ScratchpadKeyRequest>) -> String {
        guarded(move || tools::scratchpad::scratchpad_delete(&req.key))
    }

    #[tool(name = "lean_scratchpad_list", description = "Lists all keys currently stored in the persistent scratchpad.")]
    pub fn scratchpad_list(&self) -> String {
        guarded(tools::scratchpad::scratchpad_list)
    }
}

#[tool_router(router = viz_tool_router)]
impl KittyToolsServer {
    #[tool(name = "generate_accessible_table", description = "Generates a WCAG 2.2 AA compliant HTML table wrapped for iframe rendering.")]
    pub fn generate_accessible_table(&self, Parameters(req): Parameters<AccessibleTableRequest>) -> String {
        guarded(move || tools::viz::generate_accessible_table(&req.title, &req.headers, &req.rows, req.summary.as_deref()))
    }

    #[tool(name = "generate_accessible_svg", description = "Generates uncrowded, WCAG 2.2 AA compliant SVG diagrams wrapped for iframe rendering.")]
    pub fn generate_accessible_svg(&self, Parameters(req): Parameters<AccessibleSvgRequest>) -> String {
        guarded(move || {
            let steps: Option<Vec<VizStep>> = req.steps.map(|steps| {
                steps
                    .into_iter()
                    .map(|s| VizStep {
                        text: s.text.unwrap_or_default(),
                        step_type: s.step_type.unwrap_or_else(|| "process".to_string()),
                        subtitle: s.subtitle,
                    })
                    .collect()
            });
            tools::viz::generate_accessible_svg(&req.diagram_type, &req.title, &req.description, steps.as_deref())
        })
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for KittyToolsServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
}
