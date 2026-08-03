use std::sync::Arc;

use dashmap::DashMap;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, Notify, Semaphore};
use tokio::time::Instant;

use crate::agent::compaction::run_compaction;
use crate::agent::context::builder::ContextBuilder;
use crate::agent::context::stats::SessionStats;
use crate::agent::sandbox::{allowed_dirs_for_session, check_containment};
use crate::agent::summarizer::SummarizerClient;
use crate::agent::types::TimingResult;
use crate::config::{FallbackConfig, SummarizerConfig};
use crate::hitl::manager::HITLManager;
use crate::mcp::MCPManager;
use crate::models::mcp::ToolDefinition;
use crate::provider::base::{Delta, ToolCall};
use crate::provider::router::ProviderRouter;
use crate::server::events::{SSEEvent, SSEEventType};
use crate::storage::sessions;
use crate::storage::timings;

/// Keywords whose value is a schema but which are purely restrictive, so
/// deleting them is always valid (the constraint just stops applying).
const OMITTABLE_SUBSCHEMA_KEYWORDS: [&str; 8] = [
    "additionalProperties",
    "unevaluatedProperties",
    "items",
    "additionalItems",
    "unevaluatedItems",
    "contains",
    "propertyNames",
    "not",
];
/// Keywords whose value is an object mapping names to schemas.
const SUBSCHEMA_MAP_KEYWORDS: [&str; 4] =
    ["properties", "patternProperties", "$defs", "definitions"];
/// Keywords whose value is an array of schemas.
const SUBSCHEMA_LIST_KEYWORDS: [&str; 4] = ["anyOf", "allOf", "oneOf", "prefixItems"];

/// Rewrite boolean sub-schemas out of a JSON Schema, in place.
///
/// JSON Schema permits a bare boolean anywhere a schema is expected (`true` =
/// "anything", `false` = "nothing"), and schema generators reach for it
/// routinely: `schemars` renders a `serde_json::Value` field as bare `true`,
/// and Pydantic renders `dict[str, Any]` as
/// `{"type": "object", "additionalProperties": true}`.
///
/// llama.cpp's grammar-constrained tool-call parser does not accept them. It
/// aborts converting the tool list with `Unrecognized schema: true` and
/// returns HTTP 400 for the *entire request* — so one such sub-schema
/// anywhere in the list breaks every message in the session, including ones
/// that would never have called the offending tool. Ollama builds no grammar
/// from tool schemas, which is why the identical setup works there and fails
/// against llama-server.
///
/// Policing the schemas of every MCP server a user might register isn't
/// possible, so normalize on the way out instead:
///
/// * In a position where the keyword can simply be dropped, drop it. `true`
///   there is exactly equivalent to absence; `false` is a constraint so
///   exotic in a tool signature that losing it beats risking a 400 — except
///   for `additionalProperties: false`/`unevaluatedProperties: false`, which
///   are ubiquitous ("don't invent parameters"), universally understood, and
///   therefore kept.
/// * In a position that *requires* a schema (a `properties` entry, an
///   `anyOf` branch), substitute `{}` — the object spelling of "anything".
///
/// Both substitutions only ever loosen a hint the model is free to ignore
/// anyway, and neither can make the request fail.
fn sanitize_boolean_subschemas(schema: &mut Value) {
    let Some(obj) = schema.as_object_mut() else {
        return;
    };

    for kw in OMITTABLE_SUBSCHEMA_KEYWORDS {
        match obj.get_mut(kw) {
            Some(Value::Bool(false))
                if kw == "additionalProperties" || kw == "unevaluatedProperties" => {}
            Some(Value::Bool(_)) => {
                obj.remove(kw);
            }
            Some(v) => sanitize_boolean_subschemas(v),
            None => {}
        }
    }
    for kw in SUBSCHEMA_MAP_KEYWORDS {
        if let Some(Value::Object(map)) = obj.get_mut(kw) {
            for v in map.values_mut() {
                if v.is_boolean() {
                    *v = json!({});
                } else {
                    sanitize_boolean_subschemas(v);
                }
            }
        }
    }
    for kw in SUBSCHEMA_LIST_KEYWORDS {
        if let Some(Value::Array(list)) = obj.get_mut(kw) {
            for v in list.iter_mut() {
                if v.is_boolean() {
                    *v = json!({});
                } else {
                    sanitize_boolean_subschemas(v);
                }
            }
        }
    }
}

/// `decide`/`record_outcome` are called automatically by the daemon itself —
/// `adaptive_decide` once before every turn, `spawn_record_outcome` once per
/// executed tool call (see both below) — so they must NOT also be offered to
/// the model as choosable tools: a model-issued call on top of the automatic
/// ones would waste a tool-call round and, for `record_outcome`, write a
/// second, model-judged outcome alongside the mechanical one. Every other AP
/// tool (`get_state`, `list_edges`, `toggle_suggestions`, etc.) stays a
/// legitimate model-invocable introspection/control tool and is unaffected.
const AUTO_INVOKED_AP_TOOL_NAMES: &[&str] = &["decide", "record_outcome"];

/// Convert ToolDefinitions to OpenAI function-calling format, excluding the
/// tools in `AUTO_INVOKED_AP_TOOL_NAMES`. Callers that need the *unfiltered*
/// tool list (gating whether AP is connected at all, building `decide`'s own
/// `available_actions`) must keep using `active_tools` directly — only the
/// array actually sent to the provider goes through this filter.
fn llm_visible_tools_openai_format(tools: &[ToolDefinition]) -> Vec<Value> {
    let visible: Vec<ToolDefinition> = tools
        .iter()
        .filter(|t| !AUTO_INVOKED_AP_TOOL_NAMES.contains(&t.name.as_str()))
        .cloned()
        .collect();
    tools_to_openai_format(&visible)
}

/// Convert ToolDefinitions to OpenAI function-calling format.
fn tools_to_openai_format(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            // A tool whose whole schema is a boolean (or anything other than
            // an object) can't describe parameters at all — send the empty
            // object schema rather than something no backend will parse.
            let mut parameters = if t.input_schema.is_object() {
                t.input_schema.clone()
            } else {
                json!({ "type": "object", "properties": {} })
            };
            sanitize_boolean_subschemas(&mut parameters);
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": parameters,
                }
            })
        })
        .collect()
}

/// Pure: render `decide`'s Python-repr payload (see `crate::pyrepr`) into a
/// compact, model-facing hint block for tail-region injection, or `None` when
/// there are no usable hints. A malformed/unexpected payload is treated as "no
/// hints" (never a failure) — consistent with the frontend's `tryParsePyRepr`.
fn render_decide_hints(payload: &str) -> Option<String> {
    let parsed = crate::pyrepr::try_parse(payload)?;
    let hints = parsed.get("hints")?.as_array()?;
    let mut lines: Vec<String> = Vec::new();
    for h in hints.iter().filter_map(|h| h.as_object()) {
        if let Some(text) = h.get("text").and_then(|t| t.as_str()) {
            let text = text.trim();
            if !text.is_empty() {
                lines.push(format!("- {text}"));
            }
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// Pure: derive a reward + optional `error_type` from a tool's returned result
/// text, mirroring the app layer's `stream.rs` predictors (`error_type` is
/// `Some("crash")` when the tool reported an error — the only signal the
/// sidecar pins a crash TTL on). Any other content rewards 1.0 (success); a
/// reported error rewards -1.0 with the crash classification.
fn reward_from_tool_result(result: &str) -> (f64, Option<&'static str>) {
    if result.starts_with("Error") || result.starts_with("[Tool error") {
        (-1.0, Some("crash"))
    } else {
        (1.0, None)
    }
}

/// Build the assistant-role message for one turn's streamed output —
/// factored out so every path that needs to persist it (the normal
/// tool-execution flow and both budget-check early-exit branches) builds
/// the identical shape, rather than some paths building it and others
/// silently skipping it.
fn build_assistant_message(content_chunks: &[String], turn_tool_calls: &[ToolCall]) -> Value {
    let mut assistant_msg = json!({
        "role": "assistant",
        "content": content_chunks.join(""),
    });
    if !turn_tool_calls.is_empty() {
        let tool_call_values: Vec<Value> = turn_tool_calls
            .iter()
            .map(|tc| {
                json!({
                    "id": tc.id,
                    "type": tc.r#type,
                    "function": tc.function
                })
            })
            .collect();
        if let Some(obj) = assistant_msg.as_object_mut() {
            obj.insert("tool_calls".to_string(), json!(tool_call_values));
        }
    }
    assistant_msg
}

/// FNV-1a 64-bit hash, used to deterministically derive a session's pinned
/// llama-server `id_slot` (see `prompt_determinism.md`). Deliberately not
/// `std::hash::DefaultHasher` — its output isn't stable across Rust
/// versions/releases, which would defeat the point of a *stable* per-session
/// slot assignment.
fn fnv1a64(s: &str) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = FNV_OFFSET_BASIS;
    for byte in s.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Hard ceiling on accumulated content characters for a single streamed
/// turn, independent of any specific failure mode. `sampling::defaults_for`
/// now sends a finite `max_tokens` to every self-hosted provider, which
/// should make this unreachable in practice — but a provider that ignores
/// `max_tokens`, or a future model that finds a different way to loop,
/// should not be able to stream forever. Set well above any plausible
/// legitimate reply (a very long essay is a few thousand words, comfortably
/// under 20k characters) so it never fires in a healthy session.
const MAX_TURN_CONTENT_CHARS: usize = 300_000;

/// Pure predicate factored out of `AgentLoop::process_stream` so the
/// containment threshold is unit-testable without constructing a full
/// `AgentLoop` (which needs a live pool, MCP manager, summarizer, etc.).
fn exceeds_content_ceiling(content_chars: usize) -> bool {
    content_chars > MAX_TURN_CONTENT_CHARS
}

const BUDGET_TOOL: &str = "request_more_steps";
const BUDGET_SYSTEM_MESSAGE: &str =
    "[System: You have executed 20 steps. Summarize your progress, explain what \
     remains, and call request_more_steps to continue.]";
/// How many extra steps a `request_more_steps` call actually grants.
const BUDGET_EXTENSION_STEPS: i32 = 20;

/// Tool names the bundled adaptive-pathway MCP server exposes (see
/// `plugins/adaptive-pathway/src/adaptive_pathway/mcp_server.py`) — excluded
/// from the daemon's own turn-end `record_outcome` calls (recording an outcome
/// for `decide`/`record_outcome` itself would be nonsensical), and excluded
/// from the tool list handed to `decide` so AP never "chooses" its own tools.
const ADAPTIVE_PATHWAY_TOOL_NAMES: &[&str] = &[
    "decide",
    "record_outcome",
    "record_annotation",
    "get_state",
    "list_edges",
    "get_edge",
    "query_attribution",
    "list_domains",
    "toggle_suggestions",
    "health_check",
    "accept_nudge",
    "session_reflection",
    "resolve_schism",
    "session_close",
];

/// How many characters of the user message are passed to `decide`/`record`
/// calls as `context`. Must match AP's own guidance that `context` be a short
/// task summary; a full message would dominate the embedding and blur domains.
const AP_CONTEXT_MAX_CHARS: usize = 300;

/// Ceiling on how long a paused tool call waits for `/approve` before it's
/// treated as denied. Matches `hitl::manager::MAX_PENDING_AGE` — the same
/// horizon the pending-action sweep already uses to decide an approval is
/// stale, so this doesn't introduce a second, inconsistent notion of "too
/// old". Without this, a tool call from a session with no live approver
/// (recipe/scheduled runs, or an interactive session whose user just never
/// responds) would wait on `Notify::notified()` forever.
const HITL_APPROVAL_TIMEOUT: Duration = Duration::from_secs(3600);

/// Strip prompt preamble wrappers from the first user message for title derivation.
fn strip_prompt_wrappers(text: &str) -> String {
    let re_system = regex::Regex::new(r"^<system>\n.*?\n</system>\n\n").unwrap();
    let re_recipe = regex::Regex::new(r"^<recipe\b[^>]*>\n.*?\n</recipe>\n\n[^\n]*\n\n").unwrap();

    let text = re_recipe.replace(text, "").to_string();
    re_system.replace(&text, "").to_string()
}

/// Derive a session title from the first user message.
fn derive_title(text: &str) -> String {
    let text = strip_prompt_wrappers(text);
    let stripped = text.trim();
    if stripped.is_empty() {
        return String::new();
    }
    let first_line = stripped.lines().next().unwrap_or("").trim().to_string();
    // Truncate by *char* count, not byte length — `first_line[..60]` panics
    // ("byte index is not a char boundary") whenever a multi-byte UTF-8
    // character (CJK, emoji, etc.) straddles byte offset 60, which silently
    // kills the whole spawned turn task the moment a session's first
    // message is non-ASCII and long enough.
    if first_line.chars().count() > 60 {
        let truncated: String = first_line.chars().take(60).collect();
        match truncated.rsplit_once(' ') {
            Some((before, _)) => format!("{}…", before),
            None => format!("{}…", truncated),
        }
    } else {
        first_line
    }
}

/// Core agent loop: manages LLM turns, tool execution, HITL, and compaction.
pub struct AgentLoop {
    router: Arc<ProviderRouter>,
    hitl: Arc<Mutex<HITLManager>>,
    mcp: Arc<MCPManager>,
    /// Keyed by HITL `action_id`; woken by the `/approve` route (once it
    /// exists) via `record_decision` + `notify_one()` so a paused tool call
    /// can resume. Shared with whatever owns this loop (see Phase G's `Agent`).
    hitl_notifies: Arc<DashMap<String, Arc<Notify>>>,
    context: ContextBuilder,
    stats: SessionStats,
    summarizer: Arc<SummarizerClient>,
    summarizer_cfg: SummarizerConfig,
    max_concurrent_tool_calls: usize,
    /// BigTiny's own app-data directory — always allowed regardless of mode
    /// (`sandbox::allowed_dirs_for_session`'s `cache_dir` param). Threaded
    /// through from `RunOptions::data_dir` rather than using
    /// `sandbox::CACHE_DIR` directly, so it respects `BIGTINY_DATA_DIR` /
    /// Kitty's consolidated data root instead of always being `~/.bigtiny`.
    cache_dir: String,
    /// Retry/failover policy for a failed `chat_completion` call. Was
    /// entirely dead config before — a transient provider error (timeout,
    /// 5xx, rate limit) ended the whole turn immediately with no retry, even
    /// though the router already tracks multiple providers by
    /// `fallback_priority` specifically to support this.
    fallback_cfg: FallbackConfig,
}

impl AgentLoop {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        router: Arc<ProviderRouter>,
        hitl: Arc<Mutex<HITLManager>>,
        mcp: Arc<MCPManager>,
        hitl_notifies: Arc<DashMap<String, Arc<Notify>>>,
        context: ContextBuilder,
        stats: SessionStats,
        summarizer: Arc<SummarizerClient>,
        summarizer_cfg: SummarizerConfig,
        max_concurrent_tool_calls: usize,
        cache_dir: String,
        fallback_cfg: FallbackConfig,
    ) -> Self {
        Self {
            router,
            hitl,
            mcp,
            hitl_notifies,
            context,
            stats,
            summarizer,
            summarizer_cfg,
            max_concurrent_tool_calls,
            cache_dir,
            fallback_cfg,
        }
    }

    pub fn pool(&self) -> &SqlitePool {
        self.context.pool()
    }

    /// Main entry point: run the agent loop for one user message. Deltas are
    /// streamed out over `event_tx` rather than a callback, since tool calls
    /// now run concurrently and a cloned channel sender is what's safely
    /// shareable across `join_all`'d futures.
    pub async fn run(
        &mut self,
        session_id: &str,
        user_message: &str,
        event_tx: mpsc::UnboundedSender<SSEEvent>,
        provider_override: Option<&str>,
        images: Option<Vec<Value>>,
    ) {
        self.run_inner(
            session_id,
            user_message,
            &event_tx,
            provider_override,
            images,
        )
        .await;
    }

    async fn run_inner(
        &mut self,
        session_id: &str,
        user_message: &str,
        event_tx: &mpsc::UnboundedSender<SSEEvent>,
        provider_override: Option<&str>,
        images: Option<Vec<Value>>,
    ) {
        let pool = self.context.pool().clone();
        let session = match sessions::get_session(&pool, session_id).await {
            Ok(Some(s)) => s,
            _ => {
                let _ = event_tx.send(SSEEvent {
                    event_type: SSEEventType::Error,
                    content: Some(format!("Session {} not found", session_id)),
                    error_message: Some(format!("Session {} not found", session_id)),
                    session_id: Some(session_id.to_string()),
                    is_last: true,
                    ..Default::default()
                });
                return;
            }
        };

        let metadata: Value = session
            .metadata
            .as_ref()
            .and_then(|m| serde_json::from_str(m).ok())
            .unwrap_or(json!({}));

        let persona_override = metadata.get("persona_override").and_then(|v| v.as_str());
        let effective_provider: Option<String> =
            provider_override.map(String::from).or_else(|| {
                metadata
                    .get("provider")
                    .and_then(|v| v.as_str().map(String::from))
            });
        let model_override = metadata.get("model").and_then(|v| v.as_str());

        let allowed_dirs = allowed_dirs_for_session(&metadata, &self.cache_dir);
        let chat_dir = metadata.get("chat_dir").and_then(|v| v.as_str());
        let cwd = metadata.get("cwd").and_then(|v| v.as_str());

        let active_tools: Vec<ToolDefinition> = self.mcp.list_tools(None);

        // The active provider's own `context_length` (Settings → Providers →
        // Advanced) wins over the daemon-wide `token_management.max_context_tokens`
        // default when set — same override `context_length` uses below for the
        // post-turn compaction check. `.ok()` here (rather than surfacing "no
        // healthy providers" as an error) is deliberate: this is a soft budget
        // hint for context assembly, not the point where an unresolvable
        // provider should abort the turn — `run_tool_loop`'s own
        // `get_provider_id` call does that properly a few lines of control flow
        // later.
        let context_tokens_override = self
            .router
            .get_provider_id(effective_provider.as_deref())
            .ok()
            .and_then(|pid| self.router.context_length(&pid));

        // Adaptive Pathway turn-start hook: call `decide` so the model sees
        // learned tool/approach preferences *before* picking tools this turn.
        // Cache-aware by construction: the returned hints are injected into
        // the tail region (right before the new user message) via
        // `build_messages`'s `ap_hints` param — never into the stable head —
        // and a disabled/unreachable AP (or a turn where `decide` returns
        // nothing) produces `None`, i.e. zero delta to the prompt (the
        // byte-identity regression test in `context/builder.rs` guards this).
        let ap_hints = self
            .adaptive_decide(session_id, user_message, &active_tools)
            .await;

        // Build initial context
        let mut messages = match self
            .context
            .build_messages(
                session_id,
                user_message,
                persona_override,
                images.as_deref(),
                context_tokens_override,
                chat_dir,
                cwd,
                ap_hints.as_deref(),
            )
            .await
        {
            Ok(m) => m,
            Err(e) => {
                let _ = event_tx.send(SSEEvent {
                    event_type: SSEEventType::Error,
                    content: Some(e),
                    error_message: Some("Context build failed".to_string()),
                    session_id: Some(session_id.to_string()),
                    is_last: true,
                    ..Default::default()
                });
                return;
            }
        };

        // Derive and set session title
        let title = derive_title(user_message);
        if !title.is_empty() {
            let _ = sessions::update_session_name(&pool, session_id, &title).await;
            let _ = event_tx.send(SSEEvent {
                event_type: SSEEventType::SessionTitle,
                content: Some(title.clone()),
                session_id: Some(session_id.to_string()),
                ..Default::default()
            });
        }

        // Save initial user message
        if let Err(e) = self.context.save_messages(session_id, &mut messages).await {
            tracing::warn!("failed to save initial message for session {session_id}: {e}");
        }
        if let Err(e) = sessions::update_session_status(&pool, session_id, "active").await {
            tracing::warn!("failed to mark session {session_id} active: {e}");
        }

        let _ = event_tx.send(SSEEvent {
            event_type: SSEEventType::SessionStatus,
            session_id: Some(session_id.to_string()),
            content: Some("active".into()),
            ..Default::default()
        });

        // Main tool-use loop
        self.run_tool_loop(
            session_id,
            &pool,
            &allowed_dirs,
            effective_provider,
            model_override,
            messages,
            &metadata,
            &active_tools,
            user_message,
            event_tx,
        )
        .await;

        if let Err(e) = sessions::update_session_status(&pool, session_id, "idle").await {
            tracing::warn!("failed to mark session {session_id} idle: {e}");
        }

        let _ = event_tx.send(SSEEvent {
            event_type: SSEEventType::SessionStatus,
            session_id: Some(session_id.to_string()),
            content: Some("idle".into()),
            is_last: true,
            ..Default::default()
        });
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_tool_loop(
        &mut self,
        session_id: &str,
        pool: &SqlitePool,
        allowed_dirs: &[String],
        effective_provider: Option<String>,
        model_override: Option<&str>,
        mut messages: Vec<Value>,
        metadata: &Value,
        active_tools: &[ToolDefinition],
        turn_context: &str,
        event_tx: &mpsc::UnboundedSender<SSEEvent>,
    ) {
        let mut max_steps: i32 = metadata
            .get("max_steps")
            .and_then(|v| v.as_i64())
            .unwrap_or(50) as i32;
        let mut step = 0;

        loop {
            if step >= max_steps {
                let err_msg = format!("Step limit ({max_steps}) reached.");
                let _ = event_tx.send(SSEEvent {
                    event_type: SSEEventType::ToolFinish,
                    tool_name: Some("__budget__".into()),
                    tool_result: Some(err_msg.clone()),
                    session_id: Some(session_id.to_string()),
                    ..Default::default()
                });
                messages.push(json!({
                    "role": "system",
                    "content": err_msg
                }));
                if let Err(e) = self.context.save_messages(session_id, &mut messages).await {
                    tracing::warn!("failed to save messages for session {session_id}: {e}");
                }
                break;
            }

            let provider_id = match self.router.get_provider_id(effective_provider.as_deref()) {
                Ok(id) => id,
                Err(_) => {
                    let _ = event_tx.send(SSEEvent {
                        event_type: SSEEventType::Error,
                        content: Some("No healthy providers available".into()),
                        error_message: Some("No healthy providers available".into()),
                        session_id: Some(session_id.to_string()),
                        is_last: true,
                        ..Default::default()
                    });
                    return;
                }
            };

            // Progressive budget check
            let mut in_budget_check = false;
            let mut tools_for_turn = llm_visible_tools_openai_format(active_tools);
            let tool_call_count: i32 = messages
                .iter()
                .filter(|m| m.get("tool_calls").is_some() || m.get("tool_call_id").is_some())
                .count() as i32;

            if tool_call_count > 0 && tool_call_count % 20 == 0 {
                messages.push(json!({
                    "role": "system",
                    "content": BUDGET_SYSTEM_MESSAGE
                }));
                in_budget_check = true;
                tools_for_turn.push(json!({
                    "type": "function",
                    "function": {
                        "name": BUDGET_TOOL,
                        "description": "Request additional steps to continue the current task",
                        "parameters": {"type": "object", "properties": {}}
                    }
                }));
            }

            let mut provider_id = provider_id;
            let mut provider_model = self.router.resolve_model(&provider_id, model_override);

            // Retry/failover: a transient error (timeout, 5xx, rate limit)
            // used to end the whole turn on the first failure — dead
            // `fallback` config despite the router already tracking
            // multiple providers by `fallback_priority` for exactly this.
            // `enabled=false` (the default) preserves the old one-shot
            // behavior exactly (`max_attempts == 1`).
            let max_attempts = if self.fallback_cfg.enabled {
                self.fallback_cfg.max_retries + 1
            } else {
                1
            };
            let mut attempt = 0u32;
            let stream = loop {
                attempt += 1;
                // Recomputed every attempt, not once up front: fallback can
                // switch `provider_id` mid-loop (below), and each provider's
                // own `-np`/`--parallel` slot count is independent — reusing
                // an id_slot derived from a *different* provider's slot
                // count against this one would pin to a slot that may not
                // even exist there, or collide with an unrelated session.
                let id_slot = self
                    .router
                    .parallel_slots(&provider_id)
                    .filter(|&n| n > 0)
                    .map(|n| (fnv1a64(session_id) % n as u64) as i32);
                // Same reasoning as `id_slot` above: sampling is per-provider
                // (a self-hosted endpoint gets a repetition-safe floor, a
                // hosted one gets none — see `provider::sampling`), so it
                // must be re-resolved against whichever provider fallback
                // has landed on for this attempt, not cached from the first.
                let sampling = self.router.sampling(&provider_id);
                match self
                    .router
                    .chat_completion(
                        &provider_id,
                        messages.clone(),
                        Some(tools_for_turn.clone()),
                        sampling,
                        Some(provider_model.clone()),
                        id_slot,
                    )
                    .await
                {
                    Ok(s) => break s,
                    Err(e) => {
                        if attempt >= max_attempts {
                            let _ = event_tx.send(SSEEvent {
                                event_type: SSEEventType::Error,
                                error_message: Some(format!("{}", e)),
                                session_id: Some(session_id.to_string()),
                                is_last: true,
                                ..Default::default()
                            });
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(
                            self.fallback_cfg.retry_delay_ms,
                        ))
                        .await;
                        // Re-resolve — the router prefers a healthy provider,
                        // so if a background health check has since marked
                        // the one that just failed unhealthy (or another
                        // provider outranks it), this can pick a different
                        // one; otherwise it retries the same provider.
                        if let Ok(next_id) = self.router.get_provider_id(None) {
                            if next_id != provider_id {
                                let _ = event_tx.send(SSEEvent {
                                    event_type: SSEEventType::ModelFailover,
                                    content: Some(format!(
                                        "Switching from provider '{provider_id}' to '{next_id}' after error: {e}"
                                    )),
                                    session_id: Some(session_id.to_string()),
                                    ..Default::default()
                                });
                                provider_id = next_id;
                                provider_model =
                                    self.router.resolve_model(&provider_id, model_override);
                            }
                        }
                    }
                }
            };

            let (content_chunks, mut turn_tool_calls, finish_reason, turn_usage, timing) =
                self.process_stream(stream, event_tx).await;

            if finish_reason.as_deref() == Some("content_ceiling_exceeded") {
                let _ = event_tx.send(SSEEvent {
                    event_type: SSEEventType::Error,
                    error_message: Some(format!(
                        "Response exceeded {} characters without stopping, most likely a \
                         repetition loop — the reply was cut off. If this keeps happening on \
                         this provider, check Settings → Providers → Advanced for a \
                         presence-penalty override.",
                        MAX_TURN_CONTENT_CHARS
                    )),
                    session_id: Some(session_id.to_string()),
                    is_last: true,
                    ..Default::default()
                });
                return;
            }

            // Record usage
            if let Some(ref usage_val) = turn_usage {
                let input_tokens = usage_val
                    .get("input_tokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let output_tokens = usage_val
                    .get("output_tokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);

                let _ = self
                    .stats
                    .record_usage(
                        session_id,
                        input_tokens as i32,
                        output_tokens as i32,
                        &provider_id,
                        &provider_model,
                    )
                    .await;
            }

            // Emit timing
            let _ = timings::insert_timing(
                pool,
                &timings::TimingRow {
                    id: uuid::Uuid::new_v4().to_string(),
                    session_id: session_id.to_string(),
                    provider_id: Some(provider_id.clone()),
                    model: Some(provider_model.clone()),
                    ttfb_ms: Some(timing.ttfb_ms),
                    ttft_ms: Some(timing.ttft_ms),
                    generation_ms: Some(timing.generation_ms),
                    total_tokens: Some(timing.total_tokens),
                    created_at: None,
                },
            )
            .await;

            let _ = event_tx.send(SSEEvent {
                event_type: SSEEventType::LlmTiming,
                session_id: Some(session_id.to_string()),
                ttfb_ms: Some(timing.ttfb_ms),
                ttft_ms: Some(timing.ttft_ms),
                generation_ms: Some(timing.generation_ms),
                provider_id: Some(provider_id.clone()),
                model: Some(provider_model.clone()),
                total_tokens: Some(timing.total_tokens as i64),
                ..Default::default()
            });

            if let Some(ref fr) = finish_reason {
                let _ = event_tx.send(SSEEvent {
                    event_type: SSEEventType::LlmStop,
                    content: Some(fr.clone()),
                    session_id: Some(session_id.to_string()),
                    usage: turn_usage.clone(),
                    ..Default::default()
                });
            }

            // Budget check handling
            if in_budget_check {
                let has_budget = turn_tool_calls.iter().any(|tc| {
                    tc.function.get("name").and_then(|v| v.as_str()) == Some(BUDGET_TOOL)
                });
                let has_other = turn_tool_calls.iter().any(|tc| {
                    tc.function.get("name").and_then(|v| v.as_str()) != Some(BUDGET_TOOL)
                });
                messages.pop(); // Remove budget system message

                if has_other && !has_budget {
                    let err: String =
                        "Step limit reached. Call request_more_steps or stop.".to_string();
                    let _ = event_tx.send(SSEEvent {
                        event_type: SSEEventType::ToolFinish,
                        tool_name: Some("__budget__".into()),
                        tool_result: Some(err.clone()),
                        session_id: Some(session_id.to_string()),
                        ..Default::default()
                    });
                    // Persist what the model actually produced this turn
                    // (including any tool call(s) it attempted) before
                    // bouncing it back for another attempt — this used to
                    // fall straight to `continue` without ever appending
                    // the assistant message, silently discarding it: the
                    // model would have no memory of having tried, and its
                    // output was gone from history for good.
                    messages.push(build_assistant_message(&content_chunks, &turn_tool_calls));
                    messages.push(json!({"role": "system", "content": err}));
                    step += 1;
                    if let Err(e) = self.context.save_messages(session_id, &mut messages).await {
                        tracing::warn!("failed to save messages for session {session_id}: {e}");
                    }
                    continue;
                }

                if has_budget {
                    turn_tool_calls.retain(|tc| {
                        tc.function.get("name").and_then(|v| v.as_str()) != Some(BUDGET_TOOL)
                    });
                    // Actually grant the extension the model was told it got —
                    // previously this only stripped the call, so the turn
                    // still hard-stopped at the original max_steps regardless.
                    max_steps += BUDGET_EXTENSION_STEPS;
                }

                if turn_tool_calls.is_empty() {
                    // Same fix as above: this path used to `break`/`continue`
                    // without ever appending the assistant message.
                    messages.push(build_assistant_message(&content_chunks, &turn_tool_calls));
                    if let Err(e) = self.context.save_messages(session_id, &mut messages).await {
                        tracing::warn!("failed to save messages for session {session_id}: {e}");
                    }
                    if finish_reason.as_deref() == Some("stop")
                        || finish_reason.as_deref() == Some("end_turn")
                    {
                        break;
                    }
                    step += 1;
                    continue;
                }
            }

            // Add assistant message (reached for a non-budget-check turn, or
            // a budget-check turn that approved more steps and still has
            // real tool calls left to execute this same turn).
            messages.push(build_assistant_message(&content_chunks, &turn_tool_calls));

            if turn_tool_calls.is_empty() {
                if finish_reason.as_deref() == Some("stop")
                    || finish_reason.as_deref() == Some("end_turn")
                {
                    if let Err(e) = self.context.save_messages(session_id, &mut messages).await {
                        tracing::warn!("failed to save messages for session {session_id}: {e}");
                    }
                    break;
                }
                step += 1;
                continue;
            }

            // Execute tool calls concurrently (bounded by max_concurrent_tool_calls)
            let tool_results = self
                .execute_tools(session_id, &turn_tool_calls, allowed_dirs, event_tx)
                .await;

            for (tc, result) in turn_tool_calls.iter().zip(tool_results) {
                let tool_name = tc
                    .function
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                // Adaptive Pathway turn-end hook: record each executed
                // (non-AP) tool's outcome back to the sidecar with real
                // context — the learning signal the model-driven path and the
                // old context-free app-layer backstop both under-deliver. This
                // runs fire-and-forget (spawned) so it never slows the agent
                // loop, and never touches the prompt, so it can't perturb
                // prompt-prefix caching.
                self.spawn_record_outcome(session_id, tool_name, &result, turn_context);
                messages.push(json!({
                    "role": "tool",
                    "content": result,
                    "tool_call_id": tc.id,
                }));
            }
            if let Err(e) = self.context.save_messages(session_id, &mut messages).await {
                tracing::warn!("failed to save messages for session {session_id}: {e}");
            }
            step += 1;

            // Compaction check. NOTE: was previously hardcoded to context
            // length 8192 regardless of the configured model — that looked
            // like a leftover stub value rather than a deliberate choice, so
            // this now passes the real configured max context length. The
            // provider's own `context_length` (Settings → Providers →
            // Advanced) wins when set — the UI has claimed this override
            // took effect since it shipped, but nothing ever read it; fall
            // back to the daemon-wide `token_management.max_context_tokens`
            // otherwise.
            let context_length = self
                .router
                .context_length(&provider_id)
                .unwrap_or(self.context.config().max_context_tokens);
            let _ = run_compaction(
                pool,
                session_id,
                &self.summarizer,
                self.context.config(),
                &self.summarizer_cfg,
                context_length,
            )
            .await;
        }
    }

    /// Adaptive Pathway turn-start hook: `decide` which tools/approaches the
    /// model should prefer this turn, formatted as a compact hint block for
    /// tail-region injection.
    ///
    /// Cache-aware by design: the result is ONLY ever passed to
    /// `ContextBuilder::build_messages`'s `ap_hints` param (injected in the
    /// live-tail region, never the head), and any failure / no-hints / AP-not-
    /// connected yields `None` → zero delta to the prompt, so the stable
    /// prefix stays byte-identical turn over turn for KV-prefix caching.
    ///
    /// Gating: AP is considered available iff its `decide` tool is currently
    /// registered with the MCP manager (i.e. the `adaptive-pathway` server is
    /// connected). If AP is disabled in Settings, Kitty never registers it, so
    /// this is a no-op with no extra config plumbing.
    async fn adaptive_decide(
        &self,
        session_id: &str,
        user_message: &str,
        active_tools: &[ToolDefinition],
    ) -> Option<String> {
        if !active_tools.iter().any(|t| t.name == "decide") {
            return None;
        }
        let available_actions: Vec<&str> = active_tools
            .iter()
            .map(|t| t.name.as_str())
            .filter(|n| !ADAPTIVE_PATHWAY_TOOL_NAMES.contains(n))
            .collect();
        let context: String = user_message.chars().take(AP_CONTEXT_MAX_CHARS).collect();
        let args = json!({
            "session_id": session_id,
            "available_actions": available_actions.join(","),
            "context": context,
        });
        // Short timeout so a slow/stuck sidecar can't delay the turn's start —
        // a hint is best-effort, never a latency tax on every message.
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            self.mcp.execute_tool("decide", &args, None),
        )
        .await
        .ok()?;
        if result.is_error {
            return None;
        }
        render_decide_hints(&result.content)
    }

    /// Adaptive Pathway turn-end hook: fire-and-forget `record_outcome` for one
    /// executed tool, with the real result text as the reward signal and the
    /// user message as context (so learning stays domain-scoped). Spawned so it
    /// never blocks the agent loop; never touches the prompt.
    fn spawn_record_outcome(&self, session_id: &str, tool_name: &str, result: &str, context: &str) {
        if ADAPTIVE_PATHWAY_TOOL_NAMES.contains(&tool_name) {
            return;
        }
        // Only bother when `record_outcome` is actually registered (AP
        // connected) — mirror `adaptive_decide`'s gate without a second
        // `list_tools` round-trip per call: presence of `decide` in the
        // registry implies the server is up.
        if !self.mcp.has_tool("record_outcome") {
            return;
        }
        let (reward, error_type) = reward_from_tool_result(result);
        let mcp = self.mcp.clone();
        let session_id = session_id.to_string();
        let tool_name = tool_name.to_string();
        let context: String = context.chars().take(AP_CONTEXT_MAX_CHARS).collect();
        tokio::spawn(async move {
            let mut args = json!({
                "session_id": session_id,
                "action_id": tool_name,
                "reward": reward,
                "context": context,
            });
            if let Some(et) = error_type {
                args["error_type"] = json!(et);
            }
            let _ = tokio::time::timeout(
                Duration::from_secs(3),
                mcp.execute_tool("record_outcome", &args, None),
            )
            .await;
        });
    }

    async fn process_stream(
        &self,
        mut stream: Pin<Box<dyn Stream<Item = Delta> + Send>>,
        event_tx: &mpsc::UnboundedSender<SSEEvent>,
    ) -> (
        Vec<String>,
        Vec<ToolCall>,
        Option<String>,
        Option<Value>,
        TimingResult,
    ) {
        let mut content_chunks: Vec<String> = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut finish_reason: Option<String> = None;
        let mut usage: Option<Value> = None;
        let mut timing = TimingResult::default();

        let start = Instant::now();
        let mut first_token = true;
        let mut token_count = 0;
        let mut content_chars = 0usize;

        while let Some(delta) = stream.next().await {
            if first_token {
                timing.ttfb_ms = start.elapsed().as_secs_f64() * 1000.0;
                timing.ttft_ms = start.elapsed().as_secs_f64() * 1000.0;
                first_token = false;
            }

            if let Some(ref content) = delta.content {
                if !content.is_empty() {
                    content_chars += content.len();
                    content_chunks.push(content.clone());
                    token_count += 1;
                    let _ = event_tx.send(SSEEvent {
                        event_type: SSEEventType::LlmDelta,
                        content: Some(content.clone()),
                        ..Default::default()
                    });
                }
            }

            // Backstop against a genuinely unbounded reply — see
            // `MAX_TURN_CONTENT_CHARS`'s doc comment. Dropping `stream`
            // (falling out of this loop without polling it again) cancels
            // the underlying request; the caller treats this exactly like
            // any other fatal stream error.
            if exceeds_content_ceiling(content_chars) {
                finish_reason = Some("content_ceiling_exceeded".to_string());
                break;
            }

            if let Some(ref reasoning) = delta.reasoning {
                if !reasoning.is_empty() {
                    let _ = event_tx.send(SSEEvent {
                        event_type: SSEEventType::ReasoningDelta,
                        content: Some(reasoning.clone()),
                        ..Default::default()
                    });
                }
            }

            if let Some(ref tcs) = delta.tool_calls {
                for tc in tcs {
                    tool_calls.push(tc.clone());
                }
            }

            if let Some(ref fr) = delta.finish_reason {
                finish_reason = Some(fr.clone());
            }

            if let Some(ref u) = delta.usage {
                // Merge rather than replace: Anthropic splits usage across two
                // events (input/cache tokens on `message_start`, the final
                // `output_tokens` on `message_delta`) — replacing wholesale
                // on each usage-bearing Delta silently dropped whichever
                // fields arrived first the moment the second one showed up.
                let obj = usage.get_or_insert_with(|| Value::Object(serde_json::Map::new()));
                if let Some(map) = obj.as_object_mut() {
                    for (k, v) in u {
                        map.insert(k.clone(), json!(v));
                    }
                }
            }
        }

        timing.generation_ms = start.elapsed().as_secs_f64() * 1000.0;
        // Prefer the provider's own reported output-token count — `token_count`
        // is actually a count of non-empty SSE content deltas, not tokens (a
        // single delta can be a sub-token fragment or bundle several tokens
        // depending on the provider's streaming granularity), and was
        // misleadingly reported as `total_tokens` in LlmTiming/the timings
        // table. Only fall back to the delta count when the provider genuinely
        // didn't report usage.
        timing.total_tokens = usage
            .as_ref()
            .and_then(|u| u.get("output_tokens"))
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .unwrap_or(token_count);

        (content_chunks, tool_calls, finish_reason, usage, timing)
    }

    /// Run every tool call in `tool_calls` concurrently (bounded by
    /// `max_concurrent_tool_calls`), preserving call order in the returned
    /// `Vec<String>` regardless of completion order (mirrors Python's
    /// `asyncio.gather`, which is also order-preserving).
    async fn execute_tools(
        &self,
        session_id: &str,
        tool_calls: &[ToolCall],
        allowed_dirs: &[String],
        event_tx: &mpsc::UnboundedSender<SSEEvent>,
    ) -> Vec<String> {
        let semaphore = Arc::new(Semaphore::new(self.max_concurrent_tool_calls.max(1)));

        let futures = tool_calls.iter().map(|tc| {
            let tool_name = tc
                .function
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let tool_args = tc.function.get("arguments").cloned().unwrap_or(json!({}));
            let semaphore = semaphore.clone();
            self.execute_one_tool_call(
                session_id,
                tool_name,
                tool_args,
                allowed_dirs,
                semaphore,
                event_tx,
            )
        });

        futures::future::join_all(futures).await
    }

    /// Sandbox + HITL + execution for a single tool call. Never panics or
    /// returns an `Err` — every outcome is folded into the returned result
    /// string, since this runs inside a `join_all` alongside sibling calls.
    ///
    /// Ordering matches the Python reference: HITL is checked *first*; only
    /// if HITL would allow the call does a sandbox-containment failure come
    /// into play, and even then it force-escalates to human approval
    /// (`hitl.force_approval`) rather than denying outright — a containment
    /// failure alone should never be an unrecoverable dead end for the user.
    async fn execute_one_tool_call(
        &self,
        session_id: &str,
        tool_name: String,
        tool_args: Value,
        allowed_dirs: &[String],
        semaphore: Arc<Semaphore>,
        event_tx: &mpsc::UnboundedSender<SSEEvent>,
    ) -> String {
        let _ = event_tx.send(SSEEvent {
            event_type: SSEEventType::ToolStart,
            tool_name: Some(tool_name.clone()),
            tool_args: Some(tool_args.clone()),
            session_id: Some(session_id.to_string()),
            ..Default::default()
        });

        let mut decision = {
            let mut hitl = self.hitl.lock().await;
            hitl.check_tool_call(session_id, &tool_name, &tool_args)
                .await
        };

        if (decision.action == "proceed" || decision.action == "always_allow")
            && !check_containment(&tool_args, allowed_dirs)
        {
            let mut hitl = self.hitl.lock().await;
            decision = hitl.force_approval(session_id, &tool_name, &tool_args);
        }

        if decision.action == "rejected" {
            let err = format!("Tool {} denied by HITL policy", tool_name);
            let _ = event_tx.send(SSEEvent {
                event_type: SSEEventType::ToolFinish,
                tool_name: Some(tool_name.clone()),
                tool_result: Some(err.clone()),
                session_id: Some(session_id.to_string()),
                ..Default::default()
            });
            return err;
        }

        if decision.action == "needs_approval" {
            let action_id = decision.pending_action_id.clone().unwrap_or_default();

            // Register the Notify *before* emitting HitlPause — Kitty's own
            // frontend races to auto-decide/approve the instant it sees this
            // event (see `stream.rs`'s `hitl_pause` handler), so if the
            // `/approve` call's `resolve_approval` ran before this entry
            // existed, its `notify_one()` would be lost (no entry to find),
            // and the fresh `Notify` inserted afterward would then wait
            // forever with nothing left to wake it.
            let notify = self
                .hitl_notifies
                .entry(action_id.clone())
                .or_insert_with(|| Arc::new(Notify::new()))
                .clone();

            let _ = event_tx.send(SSEEvent {
                event_type: SSEEventType::HitlPause,
                tool_name: Some(tool_name.clone()),
                tool_args: Some(tool_args.clone()),
                session_id: Some(session_id.to_string()),
                action_id: Some(action_id.clone()),
                content: decision.reason.clone(),
                ..Default::default()
            });

            // Bounded wait, not an unconditional one: nothing guarantees a
            // live approver is watching this session (recipe/scheduled runs
            // call `run_turn_and_wait` with the SSE receiver discarded, so
            // `HitlPause` above has no listener that could ever call
            // `/approve`) — an unconditional `.await` here deadlocked those
            // runs permanently, with no way to cancel a `run_turn_and_wait`
            // call even from `shutdown()`. Timing out and falling through to
            // the "denied" branch below fails safe rather than silently
            // auto-executing an unattended tool call.
            let timed_out = tokio::time::timeout(HITL_APPROVAL_TIMEOUT, notify.notified())
                .await
                .is_err();
            self.hitl_notifies.remove(&action_id);

            let resolved = if timed_out {
                // Leave the pending-action record itself alone (other tool
                // calls in the same turn may have their own concurrent
                // pending approvals) — `sweep_stale` reaps it later.
                None
            } else {
                let mut hitl = self.hitl.lock().await;
                hitl.pop_decision(&action_id)
            };

            let _ = event_tx.send(SSEEvent {
                event_type: SSEEventType::HitlResolved,
                tool_name: Some(tool_name.clone()),
                session_id: Some(session_id.to_string()),
                action_id: Some(action_id.clone()),
                content: resolved.clone(),
                ..Default::default()
            });

            match resolved.as_deref() {
                Some("allow") | Some("always_allow") => {}
                _ => {
                    let err = format!("Tool {} denied by HITL policy", tool_name);
                    let _ = event_tx.send(SSEEvent {
                        event_type: SSEEventType::ToolFinish,
                        tool_name: Some(tool_name.clone()),
                        tool_result: Some(err.clone()),
                        session_id: Some(session_id.to_string()),
                        ..Default::default()
                    });
                    return err;
                }
            }
        }

        let _permit = semaphore.acquire_owned().await;
        let result = self.mcp.execute_tool(&tool_name, &tool_args, None).await;

        let output = if result.is_error {
            format!("[Tool '{}' error: {}]", tool_name, result.content)
        } else {
            result.content.clone()
        };

        let _ = event_tx.send(SSEEvent {
            event_type: SSEEventType::ToolFinish,
            tool_name: Some(tool_name.clone()),
            tool_result: Some(output.clone()),
            duration_ms: Some(result.duration_ms as i64),
            session_id: Some(session_id.to_string()),
            ..Default::default()
        });

        output
    }
}

#[cfg(test)]
mod fnv1a64_tests {
    use super::fnv1a64;

    #[test]
    fn same_input_produces_same_hash() {
        assert_eq!(fnv1a64("session-abc"), fnv1a64("session-abc"));
    }

    #[test]
    fn different_inputs_produce_different_hashes() {
        assert_ne!(fnv1a64("session-abc"), fnv1a64("session-xyz"));
    }

    #[test]
    fn known_vector_matches_fnv1a_spec() {
        // Standard FNV-1a test vector: hashing the empty string yields the
        // offset basis unchanged.
        assert_eq!(fnv1a64(""), 0xcbf29ce484222325);
    }
}

#[cfg(test)]
mod schema_sanitizer_tests {
    use super::{llm_visible_tools_openai_format, sanitize_boolean_subschemas, tools_to_openai_format};
    use crate::models::mcp::ToolDefinition;
    use serde_json::json;

    fn tool(input_schema: serde_json::Value) -> ToolDefinition {
        ToolDefinition {
            name: "t".into(),
            description: "d".into(),
            input_schema,
            server_id: "s".into(),
        }
    }

    fn named_tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: "d".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
            server_id: "adaptive-pathway".into(),
        }
    }

    /// The exact shape `kitty-tools`' `generate_accessible_table` emitted:
    /// `Vec<Vec<serde_json::Value>>` becomes `"items": true` two levels down,
    /// which is what llama-server rejected with `Unrecognized schema: true`.
    #[test]
    fn drops_a_boolean_items_keyword_nested_under_properties() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "rows": { "type": "array", "items": { "type": "array", "items": true } }
            }
        });
        sanitize_boolean_subschemas(&mut schema);
        assert_eq!(
            schema,
            json!({
                "type": "object",
                "properties": {
                    "rows": { "type": "array", "items": { "type": "array" } }
                }
            })
        );
    }

    /// The `wasm-math-mcp` shape: Pydantic's `dict[str, Any] | None`.
    #[test]
    fn drops_additional_properties_true_inside_an_anyof_branch() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "variables": {
                    "anyOf": [
                        { "type": "object", "additionalProperties": true },
                        { "type": "null" }
                    ]
                }
            }
        });
        sanitize_boolean_subschemas(&mut schema);
        assert_eq!(
            schema["properties"]["variables"]["anyOf"][0],
            json!({ "type": "object" })
        );
    }

    /// `additionalProperties: false` means "don't invent parameters" — it is
    /// ubiquitous, understood everywhere, and must survive untouched.
    #[test]
    fn keeps_additional_properties_false() {
        let mut schema = json!({ "type": "object", "additionalProperties": false });
        sanitize_boolean_subschemas(&mut schema);
        assert_eq!(
            schema,
            json!({ "type": "object", "additionalProperties": false })
        );
    }

    /// A boolean sitting where a schema is *required* can't be deleted, so it
    /// becomes `{}` instead.
    #[test]
    fn replaces_a_required_schema_position_with_an_empty_object() {
        let mut schema = json!({
            "type": "object",
            "properties": { "anything": true },
            "oneOf": [true, { "type": "string" }]
        });
        sanitize_boolean_subschemas(&mut schema);
        assert_eq!(schema["properties"]["anything"], json!({}));
        assert_eq!(schema["oneOf"][0], json!({}));
        assert_eq!(schema["oneOf"][1], json!({ "type": "string" }));
    }

    #[test]
    fn leaves_a_boolean_free_schema_untouched() {
        let original = json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        });
        let mut schema = original.clone();
        sanitize_boolean_subschemas(&mut schema);
        assert_eq!(schema, original);
    }

    #[test]
    fn tools_to_openai_format_sanitizes_and_replaces_non_object_schemas() {
        let out = tools_to_openai_format(&[
            tool(json!({ "type": "object", "properties": { "x": true } })),
            tool(json!(true)),
        ]);
        assert_eq!(
            out[0]["function"]["parameters"]["properties"]["x"],
            json!({})
        );
        assert_eq!(
            out[1]["function"]["parameters"],
            json!({ "type": "object", "properties": {} })
        );
    }

    /// `decide`/`record_outcome` are called automatically by the daemon
    /// itself (`adaptive_decide`/`spawn_record_outcome`) — they must never
    /// also reach the model as choosable tools, or a model-issued call would
    /// duplicate the automatic one. Every other AP tool stays visible.
    #[test]
    fn llm_visible_tools_hides_only_decide_and_record_outcome() {
        let tools = vec![
            named_tool("decide"),
            named_tool("record_outcome"),
            named_tool("get_state"),
            named_tool("shell_run"),
        ];
        let out = llm_visible_tools_openai_format(&tools);
        let names: Vec<&str> = out
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["get_state", "shell_run"]);
    }
}

#[cfg(test)]
mod content_ceiling_tests {
    use super::{exceeds_content_ceiling, MAX_TURN_CONTENT_CHARS};

    #[test]
    fn a_reply_far_under_the_ceiling_does_not_trip_it() {
        assert!(!exceeds_content_ceiling(20_000));
    }

    #[test]
    fn exactly_at_the_ceiling_does_not_trip_it() {
        assert!(!exceeds_content_ceiling(MAX_TURN_CONTENT_CHARS));
    }

    #[test]
    fn one_char_over_the_ceiling_trips_it() {
        assert!(exceeds_content_ceiling(MAX_TURN_CONTENT_CHARS + 1));
    }
}

#[cfg(test)]
mod adaptive_pathway_hook_tests {
    use super::{reward_from_tool_result, render_decide_hints};

    #[test]
    fn decide_hints_renders_text_from_real_payload() {
        let payload = "{'hints': [{'text': \"don't do this\", 'confidence': 0.8, 'type': 'single'}, {'text': 'use write for new files', 'confidence': 0.5, 'type': 'single'}], 'confidence': 0.6, 'novelty': 0.1, 'is_flow_state': false}";
        let out = render_decide_hints(payload);
        assert!(out.is_some());
        let text = out.unwrap();
        assert!(text.contains("don't do this"), "{text}");
        assert!(text.contains("use write for new files"), "{text}");
    }

    #[test]
    fn decide_hints_none_on_empty_or_malformed_payload() {
        assert!(render_decide_hints("{'hints': []}").is_none());
        assert!(render_decide_hints("not python").is_none());
        assert!(render_decide_hints("").is_none());
    }

    #[test]
    fn decide_hints_normalizes_and_drops_blank_entries() {
        let payload = "{'hints': [{'text': '  '}, {'text': 'use edit'}], 'confidence': 0.0}";
        let out = render_decide_hints(payload).unwrap();
        assert!(!out.contains("  "));
        assert!(out.contains("use edit"));
    }

    #[test]
    fn reward_is_positive_for_plain_output_and_negative_for_error_prefixes() {
        assert_eq!(reward_from_tool_result("file contents here"), (1.0, None));
        assert_eq!(reward_from_tool_result(""), (1.0, None));
        assert_eq!(
            reward_from_tool_result("Error: file not found"),
            (-1.0, Some("crash"))
        );
        assert_eq!(
            reward_from_tool_result("[Tool error: timeout]"),
            (-1.0, Some("crash"))
        );
    }
}

