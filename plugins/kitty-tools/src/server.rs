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
use crate::tools::viz::model as viz_model;

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

/// Hand-written schema for `AccessibleTableRequest::rows`.
///
/// The field stays `Vec<Vec<serde_json::Value>>` — a cell really can be a
/// string, a number, or a boolean, and the renderer handles all three — but
/// schemars renders `serde_json::Value` as the *boolean* schema `true`
/// ("anything"). That is legal JSON Schema and harmless to Ollama, but
/// llama.cpp builds a decoding grammar from the tool list and aborts on a
/// boolean sub-schema with `Unrecognized schema: true`, returning HTTP 400
/// for the whole request — so a single `"items": true` here took down every
/// message in the session, including ones that never touched this tool.
/// Spelling the cell type out explicitly is both grammar-safe and a truer
/// description of what belongs in a table cell than "anything" was.
///
/// BigTiny sanitizes boolean sub-schemas defensively as well
/// (`agent/loop_.rs::sanitize_boolean_subschemas`), for MCP servers we don't
/// own; this keeps the schema correct at the source for every other client.
fn rows_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "array",
        "minItems": 1,
        "items": {
            "type": "array",
            "items": {
                "anyOf": [
                    { "type": "string" },
                    { "type": "number" },
                    { "type": "boolean" }
                ]
            }
        }
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AccessibleTableRequest {
    /// Caption and main title of the table.
    pub title: String,
    /// Column header strings, e.g. ["Region", "Q1", "Q2"].
    #[schemars(length(min = 1))]
    pub headers: Vec<String>,
    /// One array per row. Every row must have exactly as many values as
    /// there are `headers`. The first value in each row becomes that row's
    /// header cell, so put the row's label there.
    #[schemars(schema_with = "rows_schema")]
    pub rows: Vec<Vec<serde_json::Value>>,
    /// Screen-reader summary of the trend, e.g. "Sales rose in every region
    /// except West."
    pub summary: Option<String>,
}

/// Kept flat (`{"type":"string","enum":[...]}`) rather than doc-commented per
/// variant: schemars 1.x switches a doc-commented unit enum to
/// `oneOf`-of-`const`, which grammar-constrained decoders (llama.cpp/Ollama)
/// handle far less reliably than a plain string enum. All guidance on what
/// each value means lives on the *field* that uses this type instead —
/// `tests/schema.rs` asserts the flat form so this can't regress silently.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VizDiagramType {
    SingleLane,
    Flowchart,
    Tree,
    Swimlane,
    JourneyMap,
}

impl VizDiagramType {
    fn to_model(self) -> viz_model::DiagramType {
        match self {
            VizDiagramType::SingleLane => viz_model::DiagramType::SingleLane,
            VizDiagramType::Flowchart => viz_model::DiagramType::Flowchart,
            VizDiagramType::Tree => viz_model::DiagramType::Tree,
            VizDiagramType::Swimlane => viz_model::DiagramType::Swimlane,
            VizDiagramType::JourneyMap => viz_model::DiagramType::JourneyMap,
        }
    }
}

/// See the doc comment on `VizDiagramType` for why this has no per-variant
/// doc comments. `#[serde(alias = "gate")]` keeps the crate's historical
/// step-type name working for any caller still using it.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VizStepType {
    Start,
    Process,
    #[serde(alias = "gate")]
    Decision,
    End,
}

impl VizStepType {
    fn to_model(self) -> viz_model::StepType {
        match self {
            VizStepType::Start => viz_model::StepType::Start,
            VizStepType::Process => viz_model::StepType::Process,
            VizStepType::Decision => viz_model::StepType::Decision,
            VizStepType::End => viz_model::StepType::End,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VizStepParam {
    /// Short unique id for this node, e.g. "a", "check", "n3". Required for
    /// "flowchart" and "tree" so that `next` can reference it. Ignored by
    /// "single_lane" and "journey_map".
    pub id: Option<String>,

    /// The label drawn inside the node. Keep it under ~40 characters; longer
    /// text wraps to at most 3 lines and then truncates. For "journey_map"
    /// this is the stage name, e.g. "Sign Up".
    pub text: String,

    /// Node shape. "start"/"end" draw rounded pill terminators, "process"
    /// draws a box, "decision" draws a gate shape with YES/NO branch labels
    /// on a "flowchart". Defaults to "process". Ignored by "journey_map" and
    /// "tree", which always draw a plain box.
    #[serde(rename = "type")]
    pub step_type: Option<VizStepType>,

    /// Small caption under the label. On a "decision" node use it for the
    /// question being asked; on a "journey_map" stage use it for what the
    /// user does there, e.g. "Fills out the signup form".
    pub subtitle: Option<String>,

    /// Which horizontal band this step belongs to, e.g. "Customer",
    /// "Backend API". Required for "swimlane"; ignored by every other
    /// diagram_type. Lanes are drawn top-to-bottom in first-seen order.
    pub lane: Option<String>,

    /// How the user feels at this stage, from -2 (very frustrated) to 2
    /// (delighted). Used only by "journey_map", where it plots the sentiment
    /// curve. Omit it on every stage to suppress the curve entirely.
    #[schemars(range(min = -2, max = 2))]
    pub sentiment: Option<i32>,

    /// A friction point at this stage, e.g. "Too many form fields". Used only
    /// by "journey_map"; drawn as a card in the pain-points row.
    pub pain: Option<String>,

    /// Ids of the node(s) this one flows into. Used by "flowchart" (branches
    /// — list two ids on a "decision" node) and "tree" (children). Omit on a
    /// terminal node. Ignored by "single_lane", "swimlane" and "journey_map",
    /// which flow in array order instead.
    pub next: Option<Vec<String>>,
}

impl VizStepParam {
    fn into_model(self) -> viz_model::Step {
        viz_model::Step {
            id: self.id,
            text: self.text,
            step_type: self.step_type.map(VizStepType::to_model).unwrap_or_default(),
            subtitle: self.subtitle,
            lane: self.lane,
            sentiment: self.sentiment,
            pain: self.pain,
            next: self.next.unwrap_or_default(),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AccessibleSvgRequest {
    /// Which diagram to draw, chosen by the shape of your data:
    /// "single_lane" is a straight A -> B -> C process with no branches
    /// (uses text/type/subtitle). "flowchart" is a process where some step
    /// has more than one possible next step (uses id/text/type/subtitle/next
    /// — give every step an `id` and list branch targets in `next`). "tree"
    /// is a hierarchy like an org chart or file layout (uses id/text/next,
    /// where `next` means "children"). "swimlane" shows who does what, steps
    /// grouped into actor bands (uses text/lane/id/next). "journey_map"
    /// shows stages of a user experience with feelings (uses
    /// text/subtitle/sentiment/pain).
    pub diagram_type: VizDiagramType,

    /// Title drawn at the top of the diagram and used as the iframe title.
    pub title: String,

    /// One or two sentences describing the diagram for screen-reader users.
    /// Emitted into the SVG `<desc>`. Say what the diagram shows, not that it
    /// is a diagram.
    pub description: String,

    /// The nodes, in reading order. At least 1 (up to 40 for most
    /// diagram_types; 24 for "swimlane"; 12 for "journey_map"). Every
    /// diagram_type requires this — there is no built-in content.
    #[schemars(length(min = 1))]
    pub steps: Vec<VizStepParam>,
}

/// See the doc comment on `VizDiagramType` for why this has no per-variant
/// doc comments.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VizChartType {
    Bar,
    HorizontalBar,
    Line,
    GroupedBar,
}

impl VizChartType {
    fn to_model(self) -> viz_model::ChartType {
        match self {
            VizChartType::Bar => viz_model::ChartType::Bar,
            VizChartType::HorizontalBar => viz_model::ChartType::HorizontalBar,
            VizChartType::Line => viz_model::ChartType::Line,
            VizChartType::GroupedBar => viz_model::ChartType::GroupedBar,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChartSeriesParam {
    /// Name of this data series, shown in the legend, e.g. "2024 Revenue".
    pub name: String,
    /// One number per entry in `categories`, in the same order. Must be
    /// exactly the same length as `categories`.
    pub values: Vec<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AccessibleChartRequest {
    /// "bar" (vertical bars), "horizontal_bar" (long category names), "line"
    /// (change over an ordered sequence like months), or "grouped_bar" (2-4
    /// series compared per category).
    pub chart_type: VizChartType,
    /// Title drawn above the chart and used as the iframe title.
    pub title: String,
    /// One or two sentences stating the takeaway for screen-reader users,
    /// e.g. "Revenue grew each quarter, with the largest jump in Q3."
    pub description: String,
    /// The category axis labels, e.g. ["Q1", "Q2", "Q3", "Q4"]. 1 to 24
    /// entries.
    #[schemars(length(min = 1, max = 24))]
    pub categories: Vec<String>,
    /// One entry for a simple chart, 2-4 for a comparison. Each series must
    /// have exactly as many `values` as there are `categories`.
    #[schemars(length(min = 1, max = 4))]
    pub series: Vec<ChartSeriesParam>,
    /// Label for the category axis, e.g. "Quarter".
    pub x_label: Option<String>,
    /// Label for the value axis, e.g. "Revenue (USD millions)".
    pub y_label: Option<String>,
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
    /// local-machine tools, plus the 3 visualization tools, included only
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
    #[tool(
        name = "generate_accessible_table",
        description = "Renders a WCAG 2.2 AA compliant HTML table inline in the chat. Use it when the individual values matter -- comparisons across more than two dimensions, or any data a reader needs to read exactly. Use generate_accessible_chart instead when the shape of the numbers is the point, not the exact values. Every row must have exactly as many values as there are headers."
    )]
    pub fn generate_accessible_table(&self, Parameters(req): Parameters<AccessibleTableRequest>) -> String {
        guarded(move || tools::viz::generate_accessible_table(&req.title, &req.headers, &req.rows, req.summary.as_deref()))
    }

    #[tool(
        name = "generate_accessible_svg",
        description = "Draws a process, hierarchy or user-journey diagram as an accessible SVG, rendered inline in the chat. Use this for anything with steps, actors, branches or stages -- pick diagram_type by the shape of your data (see its description). Do NOT use it for numeric data -- use generate_accessible_chart for that, or generate_accessible_table for raw values. Every node comes from `steps`; there is no built-in content. Example, a branching flowchart: {\"diagram_type\":\"flowchart\",\"title\":\"Login\",\"description\":\"How a login request is authenticated.\",\"steps\":[{\"id\":\"a\",\"text\":\"Receive request\",\"type\":\"start\",\"next\":[\"b\"]},{\"id\":\"b\",\"text\":\"Credentials valid?\",\"type\":\"decision\",\"next\":[\"c\",\"d\"]},{\"id\":\"c\",\"text\":\"Issue token\",\"type\":\"end\"},{\"id\":\"d\",\"text\":\"Return 401\",\"type\":\"end\"}]}"
    )]
    pub fn generate_accessible_svg(&self, Parameters(req): Parameters<AccessibleSvgRequest>) -> String {
        guarded(move || {
            let diagram_type = req.diagram_type.to_model();
            let steps: Vec<viz_model::Step> = req.steps.into_iter().map(VizStepParam::into_model).collect();
            tools::viz::generate_accessible_svg(diagram_type, &req.title, &req.description, steps)
        })
    }

    #[tool(
        name = "generate_accessible_chart",
        description = "Draws a bar or line chart from numeric data as an accessible SVG, with a hidden data table for screen readers. Use it when you have numbers per category and want to show comparison or trend; use generate_accessible_table when the exact values matter more than the shape. Every series must have one value per category. Example: {\"chart_type\":\"bar\",\"title\":\"Revenue by quarter\",\"description\":\"Revenue rose each quarter, with the largest jump in Q3.\",\"categories\":[\"Q1\",\"Q2\",\"Q3\",\"Q4\"],\"series\":[{\"name\":\"Revenue\",\"values\":[12.4,15.1,22.8,24.0]}],\"y_label\":\"USD millions\"}"
    )]
    pub fn generate_accessible_chart(&self, Parameters(req): Parameters<AccessibleChartRequest>) -> String {
        guarded(move || {
            let chart_type = req.chart_type.to_model();
            let series: Vec<viz_model::ChartSeries> =
                req.series.into_iter().map(|s| viz_model::ChartSeries { name: s.name, values: s.values }).collect();
            tools::viz::generate_accessible_chart(
                chart_type,
                &req.title,
                &req.description,
                req.categories,
                series,
                req.x_label.as_deref(),
                req.y_label.as_deref(),
            )
        })
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for KittyToolsServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
}
