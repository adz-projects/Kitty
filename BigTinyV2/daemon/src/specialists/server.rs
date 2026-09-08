//! The in-process MCP server that puts specialists in front of the model.
//!
//! This is the daemon's only MCP *server* role — everything else in `mcp::` is
//! a client dialing something else. It lives here rather than in a plugin crate
//! because its whole job is to reach back into the daemon: it needs the
//! orchestrator to run a delegate and the database to resolve which definition
//! the calling app should get.
//!
//! # One tool, not one per specialist
//!
//! An MCP connection here is daemon-lifetime and shared across every app and
//! every concurrently-streaming session (the same constraint that makes
//! `pathway` take an injected session id). Its `tools/list` is therefore
//! global, and a per-app tool surface is not expressible. So the roster reaches
//! the model two ways: the built-ins are named in `call_specialist`'s static
//! description, which covers the common case in zero round trips, and
//! `list_specialists` returns the calling app's actual set — including anything
//! the user has defined — for when the request does not obviously match one.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;

use crate::agent::orchestrator::{DelegateOutcome, DelegateRun, HostChoice, Orchestrator};
use crate::storage::{sessions, specialists as store};

/// One line naming what a delegated run actually used.
fn summarize(specialist: &str, outcome: &DelegateOutcome) -> String {
    let plural = if outcome.host.instances == 1 {
        "instance"
    } else {
        "instances"
    };
    format!(
        "{specialist} · {} {plural} · {} ({}) · {} tokens",
        outcome.host.instances, outcome.host.model, outcome.host.provider, outcome.host.tokens
    )
}

/// The tool a model calls to delegate. Named here so `agent::loop_` can inject
/// the executing session id into its arguments, and so a delegate's own
/// `tool_allow` can be checked for it.
pub const CALL_TOOL: &str = "call_specialist";
pub const LIST_TOOL: &str = "list_specialists";

/// Tool names owned by this server that need the calling session injected.
/// Both do: neither can be answered without knowing which app is asking.
pub const SESSION_SCOPED_TOOLS: [&str; 2] = [CALL_TOOL, LIST_TOOL];

/// Most refs one call may fan out over.
///
/// A model handed a directory listing will happily pass all of it, and each ref
/// is a full paid agent run — so this is a spend guard, not an efficiency one.
/// Refused above the cap rather than truncated: silently doing part of the work
/// is worse than saying the request was too big.
const MAX_FAN_OUT: usize = 32;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CallRequest {
    /// Which specialist to call.
    pub specialist: String,
    /// What you want it to do, in your own words. Be specific about the
    /// question and about what a good answer looks like.
    pub request: String,
    /// Document ids, file paths, or URLs the specialist should work from.
    #[serde(default)]
    pub refs: Option<Vec<String>>,
    /// Host-injected, never model-supplied.
    #[serde(default)]
    #[schemars(skip)]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListRequest {
    /// Host-injected, never model-supplied.
    #[serde(default)]
    #[schemars(skip)]
    pub session_id: Option<String>,
}

#[derive(Clone)]
pub struct SpecialistServer {
    pool: SqlitePool,
    orchestrator: Arc<Orchestrator>,
    tool_router: ToolRouter<Self>,
}

impl SpecialistServer {
    pub fn new(pool: SqlitePool, orchestrator: Arc<Orchestrator>) -> Self {
        Self {
            pool,
            orchestrator,
            tool_router: Self::core_tool_router(),
        }
    }

    /// Serve over an arbitrary duplex stream, as `mcp::client::connect_in_process`
    /// hands us.
    pub async fn serve_in_process<S>(self, stream: S) -> Result<(), String>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
    {
        let server = self.serve(stream).await.map_err(|e| e.to_string())?;
        server.waiting().await.map(|_| ()).map_err(|e| e.to_string())
    }

    /// Which app is asking. `None` when the session id was not injected, which
    /// can only mean a caller outside `agent::loop_`'s dispatch site.
    async fn app_for(&self, session_id: Option<&str>) -> Option<String> {
        let session_id = session_id.filter(|s| !s.is_empty())?;
        sessions::owner_of(&self.pool, session_id).await.ok().flatten()
    }

    /// A soft error, in the same envelope as a success.
    ///
    /// Failures come back as a *result*, not as an MCP error: the caller is a
    /// model mid-turn, and a protocol-level error gives it nothing to reason
    /// about or report, while `{"ok": false, "error": ...}` it can put in its
    /// own answer.
    fn refuse(message: impl Into<String>) -> String {
        json!({"ok": false, "error": message.into()}).to_string()
    }
}

#[tool_router(router = core_tool_router)]
impl SpecialistServer {
    #[tool(
        name = "call_specialist",
        description = "Delegate a self-contained piece of work to a specialist agent and get back \
a structured result. The specialist runs in its own context with its own tools, so its searching \
and reading never enters yours — use this when a task would take many tool calls to produce a \
small answer. Built-in specialists: `researcher` (answers a question from web sources, returns \
findings with citations); `summarizer` (reads long documents, returns notes and anchors); \
`locator` (finds where something lives across files, returns paths only); `extractor` (pulls named \
fields from many documents into rows); `analyst` (computes figures from spreadsheets and explains \
the method). Call `list_specialists` if none of those fit — the user may have defined others. \
Pass document ids and paths in `refs` rather than pasting content into `request`; the specialist \
shares your document cache. `summarizer` and `extractor` run one instance per ref, so passing \
ten documents processes them in parallel and returns one result per document. You can also \
issue several call_specialist calls in a single step and they will run at the same time. Do \
not delegate work that needs this conversation's context, and do not delegate anything you \
can answer directly."
    )]
    pub async fn call_specialist(&self, Parameters(req): Parameters<CallRequest>) -> String {
        let Some(app_id) = self.app_for(req.session_id.as_deref()).await else {
            return Self::refuse("this session cannot call specialists");
        };
        let Some(parent_session_id) = req.session_id.clone() else {
            return Self::refuse("this session cannot call specialists");
        };

        let name = req.specialist.trim();
        let spec = match store::resolve(&self.pool, &app_id, name).await {
            Ok(Some(s)) if s.enabled => s,
            // An unknown or disabled name answers with the real roster rather
            // than just "no": the model picked a name for a reason, and the
            // cheapest correction is showing it what it could have picked.
            Ok(_) => {
                let available = store::list_visible(&self.pool, &app_id)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|s| s.enabled)
                    .map(|s| s.name)
                    .collect::<Vec<_>>();
                return json!({
                    "ok": false,
                    "error": format!("no specialist named '{name}'"),
                    "available": available,
                })
                .to_string();
            }
            Err(e) => return Self::refuse(format!("could not look up '{name}': {e}")),
        };

        // Refs are appended to the request rather than templated into it: the
        // specialist's system prompt already tells it to work from ids and
        // paths, and a fixed trailing block is one less thing for a small model
        // to garble than an interpolated prompt would be.
        let request = req.request.trim().to_string();
        if request.is_empty() {
            return Self::refuse("`request` must say what the specialist should do");
        }
        let refs: Vec<String> = req.refs.clone().unwrap_or_default();

        // A fan-out: one delegate per ref, when the specialist declares that its
        // work really is per-document. This is the split the caller cannot
        // express by batching its own tool calls — `execute_tools` already runs
        // a step's calls concurrently, but only when the model knows how many
        // there are, and the usual case is a folder or a search result.
        let fan_out = spec.fan_out.as_deref() == Some("per_ref") && refs.len() > 1;
        if fan_out && refs.len() > MAX_FAN_OUT {
            return Self::refuse(format!(
                "{} sources is more than one call may fan out over (limit {MAX_FAN_OUT}). \
                 Narrow the list, or call the specialist more than once.",
                refs.len()
            ));
        }

        let base = |sources: &[String]| {
            let mut prompt = request.clone();
            if !sources.is_empty() {
                prompt.push_str("\n\nWork from these sources:\n");
                for r in sources {
                    prompt.push_str(&format!("- {r}\n"));
                }
            }
            prompt
        };
        let prompt = base(&refs);

        let run = DelegateRun {
            name: spec.name.clone(),
            parent_session_id,
            prompt,
            system_prompt: Some(spec.system_prompt.clone()),
            provider: spec.provider.clone(),
            model: spec.model.clone(),
            // The spawn tools are stripped rather than trusted to be absent: a
            // user-defined specialist could list them, and `Orchestrator`'s
            // depth check would then refuse mid-run instead of the definition
            // simply never having offered them.
            tool_allow: spec
                .tool_allow
                .iter()
                .filter(|t| !SESSION_SCOPED_TOOLS.contains(&t.as_str()))
                .cloned()
                .collect(),
            response_schema: spec.response_schema.clone(),
            max_steps: spec.max_steps,
            reasoning_cap: spec.reasoning_cap,
        };

        if fan_out {
            let specs: Vec<DelegateRun> = refs
                .iter()
                .map(|r| DelegateRun {
                    prompt: base(std::slice::from_ref(r)),
                    ..run.clone()
                })
                .collect();
            let outcomes = self.orchestrator.run_many(specs).await;

            let mut results = Vec::new();
            let mut succeeded = 0usize;
            let mut failed = 0usize;
            let mut host: Option<HostChoice> = None;
            let mut batch_tokens: i64 = 0;
            let mut notes: Vec<String> = Vec::new();

            for (source, outcome) in refs.iter().zip(outcomes) {
                match outcome {
                    Ok(Ok(o)) => {
                        succeeded += 1;
                        let value = serde_json::from_str::<serde_json::Value>(&o.answer)
                            .unwrap_or_else(|_| json!(o.answer));
                        results.push(json!({"source": source, "ok": true, "result": value}));
                        for n in o.notes {
                            if !notes.contains(&n) {
                                notes.push(n);
                            }
                        }
                        batch_tokens += o.host.tokens;
                        host.get_or_insert(o.host);
                    }
                    // A failure is this element's, not the batch's: nine good
                    // rows and a named miss beats an error that says nothing
                    // about the nine.
                    Ok(Err(why)) => {
                        failed += 1;
                        results.push(json!({"source": source, "ok": false, "error": why}));
                    }
                    Err(refusal) => {
                        failed += 1;
                        results.push(
                            json!({"source": source, "ok": false, "error": refusal.to_string()}),
                        );
                    }
                }
            }

            let mut host = host.unwrap_or(HostChoice {
                provider: "unknown".into(),
                model: "unknown".into(),
                instances: 0,
                tokens: 0,
            });
            host.instances = refs.len();
            // The batch's total, not one child's — what the caller wants to
            // know is what the fan-out cost, and each element reported only its
            // own.
            host.tokens = batch_tokens;

            return json!({
                "ok": succeeded > 0,
                "specialist": spec.name,
                "summary": format!(
                    "{} · {} of {} sources · {} ({}) · {} tokens",
                    spec.name, succeeded, refs.len(), host.model, host.provider, host.tokens
                ),
                "ran_on": host,
                "succeeded": succeeded,
                "failed": failed,
                "notes": notes,
                "results": results,
            })
            .to_string();
        }

        match self.orchestrator.run(run).await {
            Ok(Ok(outcome)) => {
                // The answer is already JSON when the specialist had a schema
                // (validated by `agent::loop_::finalize_structured`), so it is
                // embedded as a value rather than a string — a caller should
                // not have to parse a string out of a parsed object.
                let result = serde_json::from_str::<serde_json::Value>(&outcome.answer)
                    .unwrap_or_else(|_| json!(outcome.answer));
                json!({
                    "ok": true,
                    "specialist": spec.name,
                    // A plain sentence as well as the structured host, because
                    // the question it answers — what did this actually cost me —
                    // should be readable at a glance in the transcript rather
                    // than assembled from an object.
                    "summary": summarize(&spec.name, &outcome),
                    "ran_on": outcome.host,
                    "notes": outcome.notes,
                    "result": result,
                })
                .to_string()
            }
            Ok(Err(why)) => json!({
                "ok": false,
                "specialist": spec.name,
                "error": why,
            })
            .to_string(),
            // A refusal names the setting responsible (see
            // `subagent_pick::choose_host`), so it reaches the model verbatim
            // rather than being flattened into "unavailable".
            Err(refusal) => Self::refuse(refusal.to_string()),
        }
    }

    #[tool(
        name = "list_specialists",
        description = "List the specialists available to delegate to, with what each is for. Call \
this when a task looks delegable but none of the built-in specialists named in `call_specialist` \
obviously fits."
    )]
    pub async fn list_specialists(&self, Parameters(req): Parameters<ListRequest>) -> String {
        let Some(app_id) = self.app_for(req.session_id.as_deref()).await else {
            return Self::refuse("this session cannot call specialists");
        };
        match store::list_visible(&self.pool, &app_id).await {
            Ok(list) => {
                let entries: Vec<_> = list
                    .into_iter()
                    .filter(|s| s.enabled)
                    .map(|s| json!({"name": s.name, "description": s.description}))
                    .collect();
                json!({"ok": true, "specialists": entries}).to_string()
            }
            Err(e) => Self::refuse(format!("could not list specialists: {e}")),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SpecialistServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            instructions: Some(
                "Delegate bounded work to specialist agents that run in their own context."
                    .to_string(),
            ),
            ..Default::default()
        }
    }
}
