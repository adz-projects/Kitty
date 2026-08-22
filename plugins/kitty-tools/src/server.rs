use std::panic::{catch_unwind, AssertUnwindSafe};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::doc_store::{self, Extraction};
use crate::docx;
use crate::docx::write::WriteMode;
use crate::envelope::{error_response, success_response};
use crate::paths::{path_within_home, resolve};
use crate::query_filter::filter_by_query;
use crate::tools;
use crate::tools::viz::model as viz_model;

/// Paragraphs returned per page when no `limit` is given — same default as
/// `lean_file_read`'s `file_page_size` threshold in the Python plugin this
/// replaces the Word tools of.
const DEFAULT_PAGE_SIZE: u32 = 200;

/// Home-directory hard boundary (defense-in-depth; the daemon is the primary
/// gate). Word read/write authorize through the *resolved* path here —
/// before any filesystem access.
fn outside_home(resolved: &std::path::Path) -> Option<String> {
    if path_within_home(resolved) {
        None
    } else {
        Some(error_response(
            "PATH_OUTSIDE_HOME",
            "Path is outside the HOME directory",
            Some(&resolved.to_string_lossy()),
            Some("Only paths inside your home directory can be accessed."),
        ))
    }
}

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
pub struct DocReadChunkRequest {
    /// A `document_id` from a previous read (`lean_file_read`,
    /// `lean_word_read_text`, `lean_pdf_read_text`, `lean_pdf_read_outline`).
    pub document_id: String,
    /// Zero-based unit index to start from. Units are pages, paragraphs or
    /// lines depending on the source — the response's `unit` says which.
    pub offset: Option<u32>,
    /// Max units to return (default 200).
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DocSearchRequest {
    /// A `document_id` from a previous read.
    pub document_id: String,
    /// Keywords to find within the cached document.
    pub query: String,
    /// Zero-based match index to continue from — see `metadata.next_offset`.
    pub offset: Option<u32>,
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
    /// Markdown-lite body text: headings (`#`..`####`), bullet and numbered
    /// lists, pipe tables, `**bold**`, `*italic*`, and `[label](url)`
    /// hyperlinks (http/https/mailto only — any other scheme is written as
    /// plain text with the URL still visible).
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
pub struct ExcelInspectRequest {
    /// Path to the .xlsx (or .xls/.ods) spreadsheet file.
    pub path: String,
}

/// Docs on fields follow the `#[schemars]`-documentation rule this crate
/// enforces for small models (see `tests/schema.rs`).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExcelReadRowsRequest {
    /// Path to the .xlsx (or .xls/.ods) spreadsheet file.
    pub path: String,
    /// Sheet name to read; defaults to the workbook's first sheet.
    pub sheet: Option<String>,
    /// Excel-style cell range, e.g. "A1:C3"; defaults to the whole sheet.
    pub range_box: Option<String>,
    /// "json" (default, list of row objects) or "csv" (raw CSV text).
    pub output_format: Option<String>,
    /// Optional keyword filter over row contents.
    pub query: Option<String>,
    /// Row offset to start from (pagination / query continuation).
    pub offset: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PdfReadTextRequest {
    /// Path to the .pdf file.
    pub path: String,
    /// First page to read (1-based, default 1).
    pub start_page: Option<u32>,
    /// Last page to read (1-based, inclusive); defaults to the last page.
    pub end_page: Option<u32>,
    /// Optional keyword filter over page text.
    pub query: Option<String>,
    /// Page offset to start from (pagination / query continuation).
    pub offset: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PdfReadOutlineRequest {
    /// Path to the .pdf file.
    pub path: String,
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
            step_type: self
                .step_type
                .map(VizStepType::to_model)
                .unwrap_or_default(),
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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AccessibleMermaidRequest {
    /// Title drawn above the diagram and used as the iframe title.
    pub title: String,
    /// One or two sentences describing the diagram for screen-reader users,
    /// e.g. "The login flow branches on whether the credentials are valid."
    pub description: String,
    /// The Mermaid source to render, e.g. "flowchart TD\\nA-->B". Any Mermaid
    /// diagram type is accepted (flowchart, sequenceDiagram, classDiagram,
    /// stateDiagram-v2, erDiagram, gantt, journey, pie, mindmap, gitGraph,
    /// timeline, and more). Rendered client-side in a sandboxed iframe; if the
    /// source fails to parse, the tool returns an error card with the raw
    /// source rather than a blank frame.
    #[schemars(length(min = 1, max = 12000))]
    pub mermaid: String,
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
    /// Assembles the router from three pieces: the 17 always-on `lean_*`
    /// local-machine tools; `lean_shell`, included on every platform except
    /// Android (see `shell_tool_router`'s doc comment); and the 3
    /// visualization tools, included only when `KITTY_VIZ_ENABLED=1`. Web
    /// search (`lean_web_search` / `lean_web_search_read_chunk`) lives in the
    /// Python `kitty-docs-web` process instead — see `docs/VERSIONS.md` for
    /// why the merged Brave/DuckDuckGo search tool moved out of this crate.
    /// Per the base plan: "remove tools from the router at startup rather
    /// than registering them and failing at call time" — env/platform is
    /// fixed for the process lifetime and BigTiny restarts this server
    /// whenever its spec (and therefore its env) changes, so a disabled tool
    /// is simply never advertised rather than advertised-then-erroring,
    /// which would burn context and invite the model to call something
    /// guaranteed to fail.
    pub fn new() -> Self {
        let mut router = Self::core_tool_router();
        #[cfg(not(target_os = "android"))]
        {
            router += Self::shell_tool_router();
        }
        if std::env::var("KITTY_VIZ_ENABLED").as_deref() == Ok("1") {
            router += Self::viz_tool_router();
        }
        Self {
            tool_router: router,
        }
    }

    /// Sorted list of every currently-registered tool name — used by
    /// `tests/protocol.rs` to pin the exact tool surface. Renaming any entry
    /// here orphans adaptive-pathway's learned routing for that tool (see
    /// the base plan's "tool names are load-bearing" section), so this list
    /// must never be "tidied."
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

/// One place to turn a failed `document_id` lookup into an envelope, so both
/// chunk tools answer a stale handle identically.
///
/// A pruned record is the expected failure (the store keeps the newest 20), so
/// the hint says how to recover rather than treating it as a caller error:
/// re-read by path, which re-extracts and issues a fresh id.
fn doc_load_error(document_id: &str, err: doc_store::LoadError) -> String {
    match err {
        doc_store::LoadError::Malformed => error_response(
            "DOCUMENT_ID_NOT_FOUND",
            "Invalid document_id.",
            None,
            Some("Use a document_id exactly as returned by a previous read."),
        ),
        doc_store::LoadError::NotFound => error_response(
            "DOCUMENT_ID_NOT_FOUND",
            &format!("No cached document for document_id '{document_id}'."),
            None,
            Some(
                "This document_id may have expired (only the 20 most recent documents are kept). \
                 Read the file again by path to get a fresh one.",
            ),
        ),
        doc_store::LoadError::Unreadable(detail) => error_response(
            "DOCUMENT_READ_ERROR",
            &format!("Cannot read the cached document: {detail}"),
            None,
            Some("Read the file again by path to re-extract it."),
        ),
    }
}

#[tool_router(router = core_tool_router)]
impl KittyToolsServer {
    #[tool(
        name = "lean_word_read_text",
        description = "Reads body text from a Word .docx, reaching paragraphs inside tables and text boxes. Supports offset-based pagination and keyword query filtering."
    )]
    pub fn word_read_text(&self, Parameters(req): Parameters<WordReadTextRequest>) -> String {
        guarded(move || {
            let resolved = resolve(&req.path);
            if let Some(err) = outside_home(&resolved) {
                return err;
            }
            // Unzip and XML-parse once, cached by (path, len, mtime). This
            // used to reparse the whole .docx on every paged call and then
            // discard everything outside the window — see `doc_store`.
            let doc = match doc_store::ensure(&resolved, doc_store::UNIT_PARAGRAPH, || {
                let paragraphs = docx::read_paragraphs(&resolved)?;
                // Headings double as the outline, and they are already in
                // hand here, so it costs nothing to carry them.
                let outline: Vec<serde_json::Value> = paragraphs
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, p)| {
                        p.heading_level
                            .map(|level| json!({ "level": level, "title": p.text, "offset": idx }))
                    })
                    .collect();
                let texts: Vec<String> = paragraphs.into_iter().map(|p| p.text).collect();
                Ok::<_, docx::DocxError>(Extraction::new(texts, outline))
            }) {
                Ok((doc, _persisted)) => doc,
                Err(docx::DocxError::NotFound) => {
                    return error_response(
                        "DOCX_NOT_FOUND",
                        "Document does not exist",
                        Some(&resolved.to_string_lossy()),
                        None,
                    );
                }
                Err(docx::DocxError::Corrupt(detail)) => {
                    return error_response(
                        "DOCX_CORRUPT",
                        &format!("Cannot open docx: {detail}"),
                        Some(&resolved.to_string_lossy()),
                        None,
                    );
                }
            };
            let texts = &doc.units;
            let offset = req.offset.unwrap_or(0) as usize;

            if let Some(query) = req.query.as_deref().filter(|q| !q.trim().is_empty()) {
                let result = filter_by_query(texts, Some(query), 50, offset);
                let message = result.no_match.then(|| {
                    format!("No direct matches for query '{query}'. Showing top section.")
                });
                let mut metadata = json!({
                    "read_method": "xml_scan",
                    "document_id": doc.document_id,
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
            let (page, has_more) = doc_store::window(texts, offset, limit);
            let mut metadata = json!({
                "read_method": "xml_scan",
                "document_id": doc.document_id,
                "unit": doc.unit,
                "offset": offset,
                "total_paragraphs": total,
                "has_more": has_more,
                "outline": doc.outline,
            });
            let message = if has_more {
                metadata["next_offset"] = json!(offset + page.len());
                Some(format!(
                    "Showing paragraphs {}-{} of {total}. The whole document is already \
                     extracted and cached — continue with lean_doc_read_chunk (document_id, \
                     offset {}) or search it with lean_doc_search.",
                    offset,
                    offset + page.len(),
                    offset + page.len()
                ))
            } else {
                None
            };
            success_response(json!(page), message.as_deref(), has_more, Some(metadata))
        })
    }

    #[tool(
        name = "lean_doc_read_chunk",
        description = "Reads a window of an already-extracted document by its document_id, with no re-parsing. Use the document_id returned by lean_file_read, lean_word_read_text, lean_pdf_read_text or lean_pdf_read_outline to walk a long document instead of re-reading it by path."
    )]
    pub fn doc_read_chunk(&self, Parameters(req): Parameters<DocReadChunkRequest>) -> String {
        guarded(move || {
            let doc = match doc_store::load(&req.document_id) {
                Ok(d) => d,
                Err(e) => return doc_load_error(&req.document_id, e),
            };
            let offset = req.offset.unwrap_or(0) as usize;
            let limit = req.limit.unwrap_or(DEFAULT_PAGE_SIZE) as usize;
            let (page, has_more) = doc_store::window(&doc.units, offset, limit);

            let mut metadata = json!({
                "document_id": doc.document_id,
                "source_path": doc.source_path,
                "unit": doc.unit,
                "offset": offset,
                "total_units": doc.total_units,
                "units_available": doc.stored_units(),
                "has_more": has_more,
            });
            if has_more {
                metadata["next_offset"] = json!(offset + page.len());
            }
            success_response(
                json!(page),
                None,
                has_more || doc.extraction_truncated,
                Some(metadata),
            )
        })
    }

    #[tool(
        name = "lean_doc_search",
        description = "Keyword-searches the full text of an already-extracted document by its document_id, across the whole document rather than one page of it. Returns matching units with their positions."
    )]
    pub fn doc_search(&self, Parameters(req): Parameters<DocSearchRequest>) -> String {
        guarded(move || {
            if req.query.trim().is_empty() {
                return error_response(
                    "DOC_QUERY_EMPTY",
                    "query must not be empty",
                    None,
                    Some("Pass keywords to search for, or use lean_doc_read_chunk to read sequentially."),
                );
            }
            let doc = match doc_store::load(&req.document_id) {
                Ok(d) => d,
                Err(e) => return doc_load_error(&req.document_id, e),
            };
            let offset = req.offset.unwrap_or(0) as usize;
            let result = filter_by_query(&doc.units, Some(&req.query), 50, offset);
            let message = result.no_match.then(|| {
                format!(
                    "No direct matches for query '{}'. Showing top section.",
                    req.query
                )
            });
            let mut metadata = json!({
                "document_id": doc.document_id,
                "source_path": doc.source_path,
                "unit": doc.unit,
                "filtered_by_query": req.query,
                "total_matches": result.total_matches,
                "total_units": doc.total_units,
                "offset": offset,
            });
            if let Some(next) = result.next_offset {
                metadata["next_offset"] = json!(next);
            }
            success_response(
                json!(result.items),
                message.as_deref(),
                result.truncated || doc.extraction_truncated,
                Some(metadata),
            )
        })
    }

    #[tool(
        name = "lean_word_read_outline",
        description = "Returns the heading structure (levels 1-4) of a Word document, reaching headings inside tables and text boxes."
    )]
    pub fn word_read_outline(&self, Parameters(req): Parameters<WordReadOutlineRequest>) -> String {
        guarded(move || {
            let resolved = resolve(&req.path);
            if let Some(err) = outside_home(&resolved) {
                return err;
            }
            let paragraphs = match docx::read_paragraphs(&resolved) {
                Ok(p) => p,
                Err(docx::DocxError::NotFound) => {
                    return error_response(
                        "DOCX_NOT_FOUND",
                        "Document does not exist",
                        Some(&resolved.to_string_lossy()),
                        None,
                    );
                }
                Err(docx::DocxError::Corrupt(detail)) => {
                    return error_response(
                        "DOCX_CORRUPT",
                        &format!("Cannot open docx: {detail}"),
                        Some(&resolved.to_string_lossy()),
                        None,
                    );
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
            success_response(
                json!(outline),
                None,
                false,
                Some(json!({"read_method": "xml_scan"})),
            )
        })
    }

    #[tool(
        name = "lean_word_write_doc",
        description = "Writes a new Word document or appends to an existing one, from markdown-lite text (headings, lists, tables, bold/italic, and [label](url) hyperlinks), with WCAG accessibility structures."
    )]
    pub fn word_write_doc(&self, Parameters(req): Parameters<WordWriteDocRequest>) -> String {
        guarded(move || {
            let resolved = resolve(&req.path);
            if let Some(err) = outside_home(&resolved) {
                return err;
            }
            let mode = match req.write_mode {
                Some(WordWriteModeParam::Append) => WriteMode::Append,
                _ => WriteMode::Create,
            };
            let language = req.language.as_deref().unwrap_or("en-US");

            if matches!(mode, WriteMode::Append) && !resolved.exists() {
                return error_response(
                    "DOCX_NOT_FOUND",
                    "Document does not exist",
                    Some(&resolved.to_string_lossy()),
                    None,
                );
            }

            let mode_label = if matches!(mode, WriteMode::Append) {
                "append"
            } else {
                "create"
            };

            match docx::write::write_document(
                &resolved,
                req.doc_text.as_deref(),
                mode,
                req.title.as_deref(),
                language,
            ) {
                Ok(result) => success_response(
                    json!({"path": result.path, "mode": result.mode, "language": result.language}),
                    Some("Document saved with WCAG accessibility metadata."),
                    false,
                    None,
                ),
                Err(docx::DocxError::NotFound) => error_response(
                    "DOCX_NOT_FOUND",
                    "Document does not exist",
                    Some(&resolved.to_string_lossy()),
                    None,
                ),
                Err(docx::DocxError::Corrupt(detail)) => {
                    // A same-name file locked open in Word (PermissionError
                    // on the equivalent Python path) surfaces here as an I/O
                    // failure during the write; distinguish it so the model
                    // gets an actionable hint instead of a generic corrupt-
                    // file message for what is really a save-mode failure.
                    if detail.to_lowercase().contains("denied")
                        || detail.to_lowercase().contains("used by another process")
                    {
                        error_response(
                            "DOCX_LOCKED",
                            "Could not save the document — it may be open in Word.",
                            Some(&detail),
                            Some("Close the file in Word (or any other program with it open) and try again."),
                        )
                    } else {
                        error_response(
                            "DOCX_WRITE_ERROR",
                            &format!("Cannot {mode_label} docx: {detail}"),
                            Some(&resolved.to_string_lossy()),
                            None,
                        )
                    }
                }
            }
        })
    }

    #[tool(
        name = "lean_excel_inspect",
        description = "Returns sheet names, dimensions, and the header row for an Excel spreadsheet (.xlsx/.xls/.ods)."
    )]
    pub fn excel_inspect(&self, Parameters(req): Parameters<ExcelInspectRequest>) -> String {
        guarded(move || tools::excel::excel_inspect(&req.path))
    }

    #[tool(
        name = "lean_excel_read_rows",
        description = "Reads rows from an Excel spreadsheet (.xlsx/.xls/.ods) as structured JSON (or CSV). Supports sheet selection, a cell range, keyword query filtering, and offset pagination (default page size 500 rows)."
    )]
    pub fn excel_read_rows(&self, Parameters(req): Parameters<ExcelReadRowsRequest>) -> String {
        guarded(move || {
            tools::excel::excel_read_rows(
                &req.path,
                req.sheet.as_deref(),
                req.range_box.as_deref(),
                req.output_format.as_deref().unwrap_or("json"),
                req.query.as_deref(),
                req.offset.unwrap_or(0) as usize,
            )
        })
    }

    #[tool(
        name = "lean_pdf_read_text",
        description = "Reads text from a PDF page-by-page. Supports page ranges, keyword query filtering, and offset pagination."
    )]
    pub fn pdf_read_text(&self, Parameters(req): Parameters<PdfReadTextRequest>) -> String {
        guarded(move || {
            tools::pdf::pdf_read_text(
                &req.path,
                req.start_page,
                req.end_page,
                req.query.as_deref(),
                req.offset.unwrap_or(0) as usize,
            )
        })
    }

    #[tool(
        name = "lean_pdf_read_outline",
        description = "Returns the table-of-contents/bookmark outline of a PDF, if it has one."
    )]
    pub fn pdf_read_outline(&self, Parameters(req): Parameters<PdfReadOutlineRequest>) -> String {
        guarded(move || tools::pdf::pdf_read_outline(&req.path))
    }

    #[tool(
        name = "lean_analyze_workspace",
        description = "Lists files and folders under path (or returns metadata if path is a file)."
    )]
    pub fn analyze_workspace(
        &self,
        Parameters(req): Parameters<AnalyzeWorkspaceRequest>,
    ) -> String {
        guarded(move || {
            tools::workspace::analyze_workspace(req.path.as_deref().unwrap_or("."), req.max_depth)
        })
    }

    #[tool(
        name = "lean_file_read",
        description = "Reads lines from a text file with line numbers. Supports query filtering."
    )]
    pub fn file_read(&self, Parameters(req): Parameters<FileReadRequest>) -> String {
        guarded(move || {
            tools::fs::file_read(
                &req.path,
                req.start_line,
                req.end_line,
                req.query.as_deref(),
            )
        })
    }

    #[tool(
        name = "lean_file_write",
        description = "Overwrites (or creates) a text file with the given content."
    )]
    pub fn file_write(&self, Parameters(req): Parameters<FileWriteRequest>) -> String {
        guarded(move || {
            tools::fs::file_write(&req.path, &req.content, req.dry_run.unwrap_or(false))
        })
    }

    #[tool(
        name = "lean_file_append",
        description = "Appends content to the end of an existing text file."
    )]
    pub fn file_append(&self, Parameters(req): Parameters<FileAppendRequest>) -> String {
        guarded(move || {
            tools::fs::file_append(&req.path, &req.content, req.dry_run.unwrap_or(false))
        })
    }

    #[tool(
        name = "lean_file_replace_str",
        description = "Replaces exact string occurrences in a file."
    )]
    pub fn file_replace_str(&self, Parameters(req): Parameters<FileReplaceStrRequest>) -> String {
        guarded(move || {
            tools::fs::file_replace_str(
                &req.path,
                &req.old_str,
                &req.new_str,
                req.dry_run.unwrap_or(false),
            )
        })
    }

    #[tool(
        name = "lean_file_replace_lines",
        description = "Replaces a specific 1-indexed inclusive line range with new content."
    )]
    pub fn file_replace_lines(
        &self,
        Parameters(req): Parameters<FileReplaceLinesRequest>,
    ) -> String {
        guarded(move || {
            tools::fs::file_replace_lines(
                &req.path,
                req.start_line,
                req.end_line,
                &req.new_content,
                req.dry_run.unwrap_or(false),
            )
        })
    }

    #[tool(
        name = "lean_cache_list",
        description = "Lists files currently stored in the scratch cache directory with their sizes."
    )]
    pub fn cache_list(&self) -> String {
        guarded(tools::cache::cache_list)
    }

    #[tool(
        name = "lean_cache_view",
        description = "Reads the text content of a file previously stored in the scratch cache directory."
    )]
    pub fn cache_view(&self, Parameters(req): Parameters<CacheFilenameRequest>) -> String {
        guarded(move || tools::cache::cache_view(&req.filename))
    }

    #[tool(
        name = "lean_cache_delete",
        description = "Deletes a single file from the scratch cache directory."
    )]
    pub fn cache_delete(&self, Parameters(req): Parameters<CacheFilenameRequest>) -> String {
        guarded(move || tools::cache::cache_delete(&req.filename))
    }

    #[tool(
        name = "lean_cache_clear",
        description = "Deletes every file in the scratch cache directory and returns how many were removed."
    )]
    pub fn cache_clear(&self) -> String {
        guarded(tools::cache::cache_clear)
    }

    #[tool(
        name = "lean_scratchpad_set",
        description = "Stores a key/value pair in the persistent scratchpad for recall across turns."
    )]
    pub fn scratchpad_set(&self, Parameters(req): Parameters<ScratchpadSetRequest>) -> String {
        guarded(move || tools::scratchpad::scratchpad_set(&req.key, &req.value))
    }

    #[tool(
        name = "lean_scratchpad_get",
        description = "Retrieves a previously stored scratchpad value by key."
    )]
    pub fn scratchpad_get(&self, Parameters(req): Parameters<ScratchpadKeyRequest>) -> String {
        guarded(move || tools::scratchpad::scratchpad_get(&req.key))
    }

    #[tool(
        name = "lean_scratchpad_delete",
        description = "Deletes a key from the persistent scratchpad."
    )]
    pub fn scratchpad_delete(&self, Parameters(req): Parameters<ScratchpadKeyRequest>) -> String {
        guarded(move || tools::scratchpad::scratchpad_delete(&req.key))
    }

    #[tool(
        name = "lean_scratchpad_list",
        description = "Lists all keys currently stored in the persistent scratchpad."
    )]
    pub fn scratchpad_list(&self) -> String {
        guarded(tools::scratchpad::scratchpad_list)
    }
}

// Separated from `core_tool_router` (rather than just `#[cfg]`-gating the
// method in place) so it can be dropped from the advertised tool set on
// Android via a plain runtime condition in `KittyToolsServer::new`, matching
// the viz router below — an app-sandbox shell backed by toybox isn't a
// useful `lean_shell` for a model to drive, and it's the tool with the
// widest blast radius against the daemon's path-containment check. Kept
// compiling on every target (see `tools::shell`'s non-Windows fallback) so
// this is a registration decision, not a build one.
#[tool_router(router = shell_tool_router)]
impl KittyToolsServer {
    #[tool(
        name = "lean_shell",
        description = "Runs a shell command and returns truncated stdout/stderr. Set dry_run=True to preview without executing."
    )]
    pub async fn shell(&self, Parameters(req): Parameters<ShellRequest>) -> String {
        tools::shell::shell(&req.command, req.dry_run.unwrap_or(false)).await
    }
}

#[tool_router(router = viz_tool_router)]
impl KittyToolsServer {
    #[tool(
        name = "generate_accessible_table",
        description = "Renders a WCAG 2.2 AA compliant HTML table inline in the chat. Use it when the individual values matter -- comparisons across more than two dimensions, or any data a reader needs to read exactly. Use generate_accessible_chart instead when the shape of the numbers is the point, not the exact values. Every row must have exactly as many values as there are headers."
    )]
    pub fn generate_accessible_table(
        &self,
        Parameters(req): Parameters<AccessibleTableRequest>,
    ) -> String {
        guarded(move || {
            tools::viz::generate_accessible_table(
                &req.title,
                &req.headers,
                &req.rows,
                req.summary.as_deref(),
            )
        })
    }

    #[tool(
        name = "generate_accessible_svg",
        description = "Draws a process, hierarchy or user-journey diagram as an accessible SVG, rendered inline in the chat. Use this for anything with steps, actors, branches or stages -- pick diagram_type by the shape of your data (see its description). Do NOT use it for numeric data -- use generate_accessible_chart for that, or generate_accessible_table for raw values. Every node comes from `steps`; there is no built-in content. Example, a branching flowchart: {\"diagram_type\":\"flowchart\",\"title\":\"Login\",\"description\":\"How a login request is authenticated.\",\"steps\":[{\"id\":\"a\",\"text\":\"Receive request\",\"type\":\"start\",\"next\":[\"b\"]},{\"id\":\"b\",\"text\":\"Credentials valid?\",\"type\":\"decision\",\"next\":[\"c\",\"d\"]},{\"id\":\"c\",\"text\":\"Issue token\",\"type\":\"end\"},{\"id\":\"d\",\"text\":\"Return 401\",\"type\":\"end\"}]}"
    )]
    pub fn generate_accessible_svg(
        &self,
        Parameters(req): Parameters<AccessibleSvgRequest>,
    ) -> String {
        guarded(move || {
            let diagram_type = req.diagram_type.to_model();
            let steps: Vec<viz_model::Step> = req
                .steps
                .into_iter()
                .map(VizStepParam::into_model)
                .collect();
            tools::viz::generate_accessible_svg(diagram_type, &req.title, &req.description, steps)
        })
    }

    #[tool(
        name = "generate_accessible_chart",
        description = "Draws a bar or line chart from numeric data as an accessible SVG, with a hidden data table for screen readers. Use it when you have numbers per category and want to show comparison or trend; use generate_accessible_table when the exact values matter more than the shape. Every series must have one value per category. Example: {\"chart_type\":\"bar\",\"title\":\"Revenue by quarter\",\"description\":\"Revenue rose each quarter, with the largest jump in Q3.\",\"categories\":[\"Q1\",\"Q2\",\"Q3\",\"Q4\"],\"series\":[{\"name\":\"Revenue\",\"values\":[12.4,15.1,22.8,24.0]}],\"y_label\":\"USD millions\"}"
    )]
    pub fn generate_accessible_chart(
        &self,
        Parameters(req): Parameters<AccessibleChartRequest>,
    ) -> String {
        guarded(move || {
            let chart_type = req.chart_type.to_model();
            let series: Vec<viz_model::ChartSeries> = req
                .series
                .into_iter()
                .map(|s| viz_model::ChartSeries {
                    name: s.name,
                    values: s.values,
                })
                .collect();
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

    #[tool(
        name = "generate_accessible_mermaid",
        description = "Renders a Mermaid diagram inline in the chat. Use it when a step/edge model is too rigid: Mermaid gives you flowcharts, sequence diagrams, class diagrams, state diagrams, ER diagrams, gantt, journey maps, pie, mindmap, gitGraph, and timeline from a single source string. Preferred over generate_accessible_svg when the caller already has Mermaid source, or needs a diagram type that steps-based layout can't express. Example: {\"title\":\"Login flow\",\"description\":\"The login flow branches on whether the credentials are valid.\",\"mermaid\":\"flowchart TD\\n  A[Receive request] --> B{Credentials valid?}\\n  B -->|Yes| C[Issue token]\\n  B -->|No| D[Return 401]\"}. Rendered server-side into a static SVG shown in the chat; invalid/unsupported source returns an error instead of a blank frame."
    )]
    pub fn generate_accessible_mermaid(
        &self,
        Parameters(req): Parameters<AccessibleMermaidRequest>,
    ) -> String {
        guarded(move || {
            tools::viz::mermaid::generate_accessible_mermaid(
                &req.mermaid,
                &req.title,
                &req.description,
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
