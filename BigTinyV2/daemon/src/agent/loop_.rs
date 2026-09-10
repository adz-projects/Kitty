use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use futures::{Stream, StreamExt};
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::pin::Pin;
use tokio::sync::{mpsc, Mutex, Notify, Semaphore};
use tokio::time::Instant;

use crate::agent::compaction::run_compaction;
use crate::agent::context::builder::ContextBuilder;
use crate::agent::context::stats::SessionStats;
use crate::agent::memory::{preflight_recall, PreflightCounters};
use crate::agent::reasoning_models;
use crate::agent::sandbox::{allowed_dirs_for_session, check_containment};
use crate::agent::summarizer_chain::SummarizerChain;
use crate::agent::tokens;
use crate::agent::types::TimingResult;
use crate::config::MemoryConfig;
use crate::config::{FallbackConfig, PathwayConfig, SummarizerConfig};
use crate::error::ProviderError;
use crate::hitl::manager::HITLManager;
use crate::mcp::MCPManager;
use crate::models::mcp::ToolDefinition;
use crate::provider::base::{Delta, ToolCall};
use crate::provider::router::ProviderRouter;
use crate::provider::schema::{self as response_schema, ResponseSpec, ANTHROPIC_STRUCTURED_TOOL};
use crate::server::events::{SSEEvent, SSEEventType};
use crate::storage::hitl_rules;
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

/// Ceiling for the jittered retry backoff — no retry sleeps longer than this.
const MAX_BACKOFF_MS: u64 = 60_000;

/// Lock-free xorshift64 — the crate pulls no `rand` dependency, and this
/// only needs to be *unpredictable enough* to stop retries from colliding
/// (thundering herd), not cryptographically random.
fn next_random_u64() -> u64 {
    static STATE: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    // Seed once on first use.
    let _ = STATE.compare_exchange(0, now | 1, Ordering::Relaxed, Ordering::Relaxed);
    let mut cur = STATE.load(Ordering::Relaxed);
    loop {
        let next = cur ^ (cur << 13);
        let next = next ^ (next >> 7);
        let next = next ^ (next << 17);
        match STATE.compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(actual) => cur = actual,
        }
    }
}

/// Partial-jitter exponential backoff in milliseconds for a retry `attempt`
/// (1-based) with a `retry_delay_ms` base: the cap doubles each attempt up to
/// `MAX_BACKOFF_MS`, and the actual sleep is a random value in
/// `[cap/2, cap)` — guaranteed minimum plus spread, so concurrent failures
/// don't all retry on the same tick.
fn backoff_ms(retry_delay_ms: u64, attempt: u32) -> u64 {
    let base = retry_delay_ms.max(1);
    let cap = base
        .saturating_mul(1u64 << attempt.saturating_sub(1).min(16))
        .min(MAX_BACKOFF_MS);
    let half = cap / 2;
    if half == 0 {
        return cap;
    }
    half + next_random_u64() % half
}

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

/// AP recall/record are handled automatically and in-process by the daemon
/// itself — `pathway_recall` before every turn and a coalesced turn-end
/// observation pass (both below) — so nothing is auto-invoked as a model
/// tool; the in-process `pathway` server's `record`/`forget` tools stay
/// legitimate model-invocable introspection/control tools and are sent to
/// the provider like any other. There used to be an
/// `llm_visible_tools_openai_format` wrapper here that filtered
/// `active_tools` against an `AUTO_INVOKED_AP_TOOL_NAMES` list before
/// formatting -- with that list permanently empty (nothing is auto-invoked
/// anymore), the wrapper was a no-op that still cloned and re-collected the
/// full tool list every single turn. Callers now call
/// `tools_to_openai_format` directly.
///
/// Write-capable MCP tools the model can reach for (bundled kitty-tools
/// plugins). A write-class call whose path resolves outside the session's
/// allowed dirs is hard-denied (see `execute_one_tool_call`), never escalated
/// to approval. Read/analysis tools (`lean_file_read`, `lean_excel_*`,
/// `lean_pdf_*`, `lean_analyze_workspace`, …) deliberately stay out of this —
/// an out-of-dir *read* is still worth surfacing to the user, not silently
/// blocked. `lean_shell` is the boundary case: only a command containing an
/// absolute path outside the allowed set is caught (`extract_shell_paths`'s
/// regex), so relative/cwd-relative writes pass through as before — a
/// documented limitation of path extraction, not a hole we can close here.
const WRITE_TOOL_NAMES: &[&str] = &[
    "lean_file_write",
    "lean_file_append",
    "lean_file_replace_str",
    "lean_file_replace_lines",
    "lean_cache_delete",
    "lean_cache_clear",
    "lean_scratchpad_set",
    "lean_scratchpad_delete",
    "lean_shell",
];

/// True if `tool_name` is a classified write-capable tool.
pub fn is_write_tool(tool_name: &str) -> bool {
    WRITE_TOOL_NAMES.contains(&tool_name)
}

/// Convert ToolDefinitions to OpenAI function-calling format.
fn tools_to_openai_format(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        // Same filter `tool_to_anthropic` has applied all along, which this
        // path lacked: a tool advertised as `"name": ""` is not usable by any
        // model, 400s the request on stricter gateways, and is a plausible way
        // to end up with a model echoing an empty name back in a tool call.
        .filter(|t| {
            if t.name.trim().is_empty() {
                tracing::warn!("dropping a tool with no name from the OpenAI-format request");
                return false;
            }
            true
        })
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

/// Pure: build the assistant-role message for one turn's streamed output —
/// factored out so every path that needs to persist it (the normal
/// tool-execution flow and both budget-check early-exit branches) builds
/// the identical shape, rather than some paths building it and others
/// silently skipping it.
/// The `tool_allow` list from a session's metadata, or `None` when it has none.
///
/// An empty array is *not* `None`: a definition that names no tools means a
/// run with no tools, and collapsing that to "unrestricted" would hand the
/// most restricted run the widest surface.
fn parse_tool_allow(metadata: &Value) -> Option<std::collections::HashSet<String>> {
    let entries = metadata.get("tool_allow")?.as_array()?;
    Some(
        entries
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::to_string)
            .collect(),
    )
}

/// The sampling fields that can change an answer, as a value for the cache key.
///
/// `SamplingParams` is not `Serialize`, and should not become so for this: the
/// key must cover exactly what alters the response and nothing that merely
/// alters transport. Missing a field here would serve a response generated under
/// different settings, which is worse than a miss because it looks like success.
fn sampling_fingerprint(s: &crate::provider::base::SamplingParams) -> Value {
    json!({
        "temperature": s.temperature,
        "top_p": s.top_p,
        "top_k": s.top_k,
        "min_p": s.min_p,
        "presence_penalty": s.presence_penalty,
        "frequency_penalty": s.frequency_penalty,
        "max_tokens": s.max_tokens,
        "effort": s.effort.as_ref().and_then(|e| e.wire_level()),
        "reasoning_max_tokens": s.reasoning_max_tokens,
    })
}

/// Drain a schema-constrained response stream into `(text, forced_tool_args)`.
///
/// Simpler than `process_stream` on purpose: this request carries no tools of
/// its own, cannot fail over, and produces no timings — the only thing to
/// recover is the answer, in whichever of the two places the dialect put it.
async fn drain_structured_answer(
    mut stream: Pin<Box<dyn Stream<Item = Delta> + Send>>,
) -> (String, Option<Value>) {
    let mut text = String::new();
    let mut forced: Option<Value> = None;
    while let Some(delta) = stream.next().await {
        if let Some(c) = delta.content {
            text.push_str(&c);
        }
        for tc in delta.tool_calls.into_iter().flatten() {
            if tc.function.get("name").and_then(|v| v.as_str()) != Some(ANTHROPIC_STRUCTURED_TOOL) {
                continue;
            }
            // `arguments` is a JSON string on the OpenAI-shaped wire and an
            // object on Anthropic's; accept either rather than assuming the
            // dialect that produced this directive.
            forced = match tc.function.get("arguments") {
                Some(Value::String(raw)) => serde_json::from_str(raw).ok(),
                Some(v) => Some(v.clone()),
                None => None,
            };
        }
    }
    (text, forced)
}

fn build_assistant_message(content: &str, turn_tool_calls: &[ToolCall]) -> Value {
    let mut assistant_msg = json!({
        "role": "assistant",
        "content": content,
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

/// Pure: synthetic `tool`-role messages for the budget-abort branch, one per
/// tool call the model issued but never executed.
///
/// The assistant message persisted in that branch carries `tool_calls` with
/// no `tool` result following them — some OpenAI-compatible backends reject a
/// subsequent request outright (HTTP 400: "tool_calls ... must be followed by
/// a tool role message") when the per-message pairing is violated. Emit a
/// single error result per pending call so the protocol invariant holds *and*
/// the model can see what it attempted was cut off.
fn build_aborted_tool_results(tool_calls: &[ToolCall], reason: &str) -> Vec<Value> {
    tool_calls
        .iter()
        .map(|tc| {
            json!({
                "role": "tool",
                "content": reason,
                "tool_call_id": tc.id,
            })
        })
        .collect()
}

/// The original hardcoded reason, kept as a constant so the one existing call
/// site reads the same as it did before `build_aborted_tool_results` was
/// widened to take one.
const ABORTED_FOR_STEP_BUDGET: &str =
    "[Tool call cancelled: the step budget was exhausted before this call could be executed.]";

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

/// Synthetic tool name for the reasoning-budget notice.
///
/// Distinct from `BUDGET_TOOL`'s `__budget__`, which Kitty's stream layer
/// suppresses — this one is meant to be visible, because a delegate that
/// stopped thinking part-way is something the reader should be able to see in
/// the transcript.
const REASONING_BUDGET_TOOL: &str = "__reasoning_budget__";

/// Output room held back for a delegate's structured report when no schema-
/// derived figure is available, and the bounds that figure is clamped to. A
/// twenty-field schema genuinely needs more room than a three-field one, so the
/// reserve is derived from the schema rather than fixed — but never so small
/// that the answer cannot be written, nor so large that it eats the whole
/// window.
const REPORT_RESERVE_FLOOR: i32 = 1024;
const REPORT_RESERVE_CEILING: i32 = 4096;

/// One completed streamed attempt: the assembled content chunks, tool calls,
/// finish reason, usage, and timing — or a `ProviderError` (including
/// mid-stream failures, which `process_stream` surfaces the same way as a
/// pre-stream `chat_completion` error so both ride one retry/failover budget).
type TurnStreamResult = Result<
    (
        String,
        Vec<ToolCall>,
        Option<String>,
        Option<Value>,
        TimingResult,
    ),
    ProviderError,
>;

/// Pure predicate factored out of `AgentLoop::process_stream` so the
/// containment threshold is unit-testable without constructing a full
/// `AgentLoop` (which needs a live pool, MCP manager, summarizer, etc.).
fn exceeds_content_ceiling(content_chars: usize) -> bool {
    content_chars > MAX_TURN_CONTENT_CHARS
}

/// Everything the model actually generated for one call, thinking included.
///
/// Endpoints disagree about `reasoning_tokens`, and the disagreement is not
/// detectable from the field alone. OpenAI's spec counts them *inside*
/// `completion_tokens` and reports the breakdown for information; plenty of
/// OpenAI-compatible servers instead report `completion_tokens` as the visible
/// completion only, with thinking accounted separately. Summing blindly double
/// counts on the first kind; ignoring them undercounts badly on the second —
/// which is what made a reasoning model's measured tokens/sec come out around a
/// third of what the server itself reported.
///
/// The one thing that *is* decidable: reasoning tokens cannot exceed a total
/// they are part of. So `reasoning >= output` proves they are being reported
/// outside it, and only then are they added. A server that excludes them but
/// happens to think less than it says is still undercounted; that is a narrower
/// wrong answer than either blanket rule, and it never over-reports.
fn output_tokens_including_reasoning(usage: &Value) -> Option<i32> {
    // Saturating casts, not `as i32` (which wraps an absurd reported value
    // negative) — same pattern as `record_usage`.
    let clamp = |v: i64| i32::try_from(v).unwrap_or(i32::MAX);
    let output = clamp(usage.get("output_tokens").and_then(|v| v.as_i64())?);
    let reasoning = usage
        .get("reasoning_tokens")
        .and_then(|v| v.as_i64())
        .map(clamp)
        .unwrap_or(0);
    if reasoning >= output {
        Some(output.saturating_add(reasoning))
    } else {
        Some(output)
    }
}

const BUDGET_TOOL: &str = "request_more_steps";
const BUDGET_SYSTEM_MESSAGE: &str =
    "[System: You have executed 20 steps. Summarize your progress, explain what \
     remains, and call request_more_steps to continue.]";
/// How many extra steps a `request_more_steps` call actually grants.
const BUDGET_EXTENSION_STEPS: i32 = 20;
/// Ceiling (inclusive) for a session's `max_steps` metadata value, clamped at
/// the top of `run_tool_loop`. Prevents a pathological/negative `max_steps`
/// from making `step >= max_steps` true on the very first iteration (ending
/// the turn before it starts) while still bounding runaway loops.
const MAX_STEPS_CEILING: i64 = 10_000;

/// Synthetic tool name for the wrap-up valve's notice. Deliberately NOT
/// `__budget__`: Kitty suppresses that one (`bigtiny/stream.rs`) because the
/// step-budget nudge is internal bookkeeping the user has no stake in. Running
/// out of *context* is the opposite — it ends the turn early and the answer is
/// visibly shorter than it would have been, so the user needs told why.
const CONTEXT_BUDGET_TOOL: &str = "__context_budget__";

/// Injected as a system message on the one request that carries no tools.
///
/// It has three jobs and each clause earns its place: forbid tool calls (some
/// models emit one from habit even when none are offered, and any it emits are
/// discarded), demand brevity (`max_tokens` is clamped to at most
/// `WRAPUP_MAX_TOKENS_CEILING`, so an overrun is truncated mid-sentence), and
/// state what remains — that last part lands in the transcript, where the
/// summarizer folds it into the `current_task_state` memory slot and the user's
/// next turn picks it up after compaction has reclaimed room.
/// Room the pre-flight guard keeps for the reply itself. Smaller than the
/// wrap-up reserve on purpose: by the time the guard is deciding, the wrap-up
/// valve has already clamped `max_tokens` to at most
/// `WRAPUP_MAX_TOKENS_CEILING`, and the question is no longer "is there room
/// to work" but "will this request be rejected outright".
const PREFLIGHT_OUTPUT_FLOOR: i32 = 1024;

/// The model name a provider will actually use, for user-facing copy.
fn provider_model_for(
    router: &ProviderRouter,
    provider_id: &str,
    model_override: Option<&str>,
) -> String {
    router.resolve_model(provider_id, model_override)
}

const WRAPUP_SYSTEM_MESSAGE: &str =
    "[System: This conversation is close to the model's context limit, so no tools \
     are available for this reply and this is the final step of the turn. Do not \
     attempt any tool calls. Give the best answer you can from what you already \
     have, briefly and directly. If anything still needs checking, say plainly \
     what it is and that it will need a follow-up turn to verify.]";

/// Output budget for the wrap-up reply. The floor keeps `max_tokens` a positive
/// integer Anthropic will accept even when the window is already overshot; the
/// ceiling is comfortably more than a closing paragraph while staying small
/// enough that the reply itself can't push the request over the limit.
const WRAPUP_MAX_TOKENS_FLOOR: i32 = 512;
const WRAPUP_MAX_TOKENS_CEILING: i32 = 2048;

/// Which of the two mutually exclusive budget interventions applies to an
/// iteration.
///
/// Extracted so the precedence decision is assertable in a test rather than
/// living implicitly in an `if/else if` that a later refactor could flatten
/// back into two independent `if`s — which is exactly the shape that would
/// reintroduce the incoherent state (offering `request_more_steps` on the same
/// request that withdraws every tool).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnMode {
    Normal,
    StepNudge,
    WrapUp,
}

/// Context exhaustion outranks the step nudge, always.
///
/// The two interventions contradict each other on the wire — the nudge's whole
/// purpose is to *append* `request_more_steps` to the tool list while wrap-up
/// *empties* it — and in prose, one saying "call request_more_steps to
/// continue" while the other says stop now. And even reconciled, granting 20
/// more *steps* is a non-answer when the exhausted resource is *context*: more
/// steps against a full window is precisely what this valve exists to prevent.
///
/// Note there is deliberately no `step > 0` guard on the wrap-up arm. The
/// condition is reachable at step 0, because `ContextBuilder` budgets against
/// the daemon-wide `max_context_tokens` while this checks the provider's own
/// window — so a provider with a smaller real window starts the turn already
/// over. Suppressing the valve there would convert a graceful degradation into
/// the hard provider 400 it exists to avoid.
fn decide_turn_mode(step: i64, wrapup_issued: bool, wrapup_due: bool) -> TurnMode {
    if wrapup_due && !wrapup_issued {
        TurnMode::WrapUp
    } else if step > 0 && step % 20 == 0 {
        TurnMode::StepNudge
    } else {
        TurnMode::Normal
    }
}

/// Pure: how a wrap-up turn's output is persisted.
///
/// Tool calls are **stripped**, not paired with synthetic results. A model can
/// still emit one despite being offered none (habit, or a proxy injecting its
/// own list), and `save_messages` would write those `tool_calls` to the DB —
/// where, with no `tool` role following them, they are a hard 400 on the *next*
/// turn's first request. The "the model would have no memory of having tried"
/// argument that justifies keeping them in the step-budget branch does not
/// apply here: that branch `continue`s and the model gets another attempt in
/// the same turn, this one `break`s. Stripping is also the smaller write, which
/// matters when the whole reason we are here is that the history is too big.
///
/// The empty-content guard is not theoretical: strip the calls from a reply
/// that was *only* a tool call and the result is `{"content": ""}` with no
/// `tool_calls`, which several backends reject outright.
fn wrapup_persist_shape(text: &str, turn_tool_calls: &[ToolCall]) -> Value {
    let content = if text.trim().is_empty() {
        if turn_tool_calls.is_empty() {
            "[No reply: the turn ended at the context limit.]"
        } else {
            "[The turn ended at the context limit before this step could run.]"
        }
    } else {
        text
    };
    json!({ "role": "assistant", "content": content })
}

/// Best-effort budget for embedding the current user message at turn-start
/// recall. Bounds the latency tax of query grounding — a timeout degrades to
/// empty-vector (weight-only, still-capped) selection rather than a failure.
const AP_RECALL_EMBED_BUDGET_MS: u64 = 1500;

/// Ceiling on how long a paused tool call waits for `/approve` before it's
/// treated as denied. Matches `hitl::manager::MAX_PENDING_AGE` — the same
/// horizon the pending-action sweep already uses to decide an approval is
/// stale, so this doesn't introduce a second, inconsistent notion of "too
/// old". Without this, a tool call from a session with no live approver
/// (recipe/scheduled runs, or an interactive session whose user just never
/// responds) would wait on `Notify::notified()` forever.
const HITL_APPROVAL_TIMEOUT: Duration = Duration::from_secs(3600);

/// Strip prompt preamble wrappers from the first user message for title
/// derivation.
///
/// `RE_RECIPE` is historical: nothing produces a `<recipe>` wrapper now that
/// specialists replaced recipes (a delegate's request never reaches a user
/// message at all). Sessions recorded before that still carry one, and a title
/// derived from the wrapper instead of the user's own words is exactly the bad
/// title this function exists to avoid -- so the pattern stays.
fn strip_prompt_wrappers(text: &str) -> String {
    // Compiled once rather than on every call: `summarizer_title` runs this
    // per message across the title fold, and `Regex::new` is not cheap.
    static RE_SYSTEM: Lazy<regex::Regex> =
        Lazy::new(|| regex::Regex::new(r"^<system>\n.*?\n</system>\n\n").unwrap());
    static RE_RECIPE: Lazy<regex::Regex> =
        Lazy::new(|| regex::Regex::new(r"^<recipe\b[^>]*>\n.*?\n</recipe>\n\n[^\n]*\n\n").unwrap());

    let text = RE_RECIPE.replace(text, "").to_string();
    RE_SYSTEM.replace(&text, "").to_string()
}

/// Strip leading `--- <label> ---\n<content>` attachment/paste blocks (see
/// `chatStore.ts`'s inlined-attachment prompt building — a dropped file or a
/// long paste becomes exactly this marker followed by its content, before
/// whatever the user actually typed) from the front of a message before
/// deriving a title from it. Without this, the *label* — a filename like
/// `lec11-remediated.pdf`, or `Pasted text — 83 words` — became the title
/// verbatim, dashes included: it describes how the content arrived, not
/// what the message is about, so it's a bad title even stripped of the
/// dashes; the fix is to skip past it and use whatever real text follows,
/// not just deverticked-decorate it.
///
/// Blocks are `\n\n`-joined (matching `chatStore.ts`), so each leading block
/// whose first line matches the marker pattern is skipped in turn, stopping
/// at the first block that isn't one — i.e. the user's own typed text, if
/// any. An attachment-only message (nothing typed) reduces to an empty
/// string, same as any other message with nothing left to title from —
/// `derive_title`'s existing empty check already leaves the session
/// unnamed rather than write a blank/junk title.
fn strip_leading_attachment_markers(text: &str) -> String {
    static MARKER: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"^--- .+ ---$").unwrap());
    let marker = &*MARKER;
    let mut rest = text;
    loop {
        let trimmed = rest.trim_start();
        let first_line = trimmed.split('\n').next().unwrap_or("");
        if !marker.is_match(first_line) {
            return trimmed.to_string();
        }
        match trimmed.find("\n\n") {
            Some(idx) => rest = &trimmed[idx + 2..],
            None => return String::new(),
        }
    }
}

/// Derive a session title from the first user message.
fn derive_title(text: &str) -> String {
    let text = strip_prompt_wrappers(text);
    let text = strip_leading_attachment_markers(&text);
    let stripped = text.trim();
    if stripped.is_empty() {
        return String::new();
    }
    let first_line = stripped.lines().next().unwrap_or("").trim();
    truncate_title(&truncate_title_words(first_line))
}

/// Titles are capped at five words. Short enough to scan a sidebar of them
/// at a glance, which is the only job a session title has — the 60-char cap
/// below is a *byte-safety* limit on top of this, not a substitute for it (a
/// single five-word line can still be long, and one unbroken 200-char "word"
/// passes the word cap untouched).
const MAX_TITLE_WORDS: usize = 5;

/// Keep at most [`MAX_TITLE_WORDS`] words, marking the cut with an ellipsis
/// so a clipped title doesn't read as a complete one. Also collapses runs of
/// whitespace, since it rebuilds the string from its words.
fn truncate_title_words(s: &str) -> String {
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.len() <= MAX_TITLE_WORDS {
        return words.join(" ");
    }
    format!("{}…", words[..MAX_TITLE_WORDS].join(" "))
}

/// Cap a title at 60 characters, breaking on a word boundary where possible.
/// Shared by `derive_title` (the naive first-line fallback) and
/// `sanitize_title` (the summarizer-derived title, release-fixes item 12) —
/// both need the exact same limit and shouldn't drift apart.
///
/// Truncates by *char* count, not byte length — `s[..60]` panics ("byte
/// index is not a char boundary") whenever a multi-byte UTF-8 character
/// (CJK, emoji, etc.) straddles byte offset 60, which would otherwise
/// silently kill the whole spawned turn/title task the moment a title is
/// non-ASCII and long enough.
fn truncate_title(s: &str) -> String {
    if s.chars().count() > 60 {
        let truncated: String = s.chars().take(60).collect();
        match truncated.rsplit_once(' ') {
            Some((before, _)) => format!("{}…", before),
            None => format!("{}…", truncated),
        }
    } else {
        s.to_string()
    }
}

/// Post-turn, summarizer-derived title (release-fixes item 12) — the
/// primary path now; `derive_title` above is the last-resort fallback for
/// when every summarizer leg fails or produces nothing usable. Runs
/// detached in its own spawned task after the turn has already completed
/// (see the call site in `run_tool_loop`), so it re-fetches the session's
/// own persisted messages rather than reusing anything off the caller's
/// stack.
async fn derive_and_set_title(
    pool: &SqlitePool,
    session_id: &str,
    summarizer: &SummarizerChain,
    provider_id: Option<&str>,
    model: Option<String>,
    event_tx: &mpsc::UnboundedSender<SSEEvent>,
) {
    let Ok(Some(row)) = sessions::get_session(pool, session_id).await else {
        return;
    };
    if row.name.as_deref().is_some_and(|n| !n.is_empty()) {
        return; // named by something else (e.g. a rename) while this was queued
    }

    let title = match summarizer_title(pool, session_id, summarizer, provider_id, model).await {
        Some(t) => t,
        None => {
            // Every summarizer leg failed — fall back to the same naive
            // first-line derivation the send-time path used to do,
            // sourced from the session's own persisted first message
            // rather than a caller-supplied string (this runs detached,
            // long after the original `user_message` argument existed).
            match crate::storage::messages::get_first_user_message(pool, session_id).await {
                Ok(Some(m)) => derive_title(&m.content.unwrap_or_default()),
                _ => return,
            }
        }
    };
    if title.is_empty() {
        return;
    }
    let _ = sessions::update_session_name(pool, session_id, &title).await;
    let _ = event_tx.send(SSEEvent {
        event_type: SSEEventType::SessionTitle,
        content: Some(title.clone()),
        session_id: Some(session_id.to_string()),
        ..Default::default()
    });
}

/// Ask the summarizer chain for a title describing what the user opened the
/// session to do. `None` on any failure — no first message yet, every
/// summarizer leg erroring, or a response with no usable `title` field — so
/// the caller falls back to the naive derivation instead of surfacing an
/// error anywhere a user could see it.
///
/// Titled from the **first user message alone**, not the last N messages.
/// The title names the session in a sidebar, so it has to describe why the
/// session exists; feeding in the tail of the exchange let it drift onto
/// whatever the assistant happened to be doing at the end of the turn, which
/// is exactly the thing a user scanning the list is not looking for. It also
/// keeps the prompt small and stable, which matters for the small local
/// model that usually answers it.
async fn summarizer_title(
    pool: &SqlitePool,
    session_id: &str,
    summarizer: &SummarizerChain,
    provider_id: Option<&str>,
    model: Option<String>,
) -> Option<String> {
    let row = crate::storage::messages::get_first_user_message(pool, session_id)
        .await
        .ok()??;
    // Strip the same leading `--- <label> ---` attachment/paste markers
    // `derive_title`'s naive fallback strips (see its doc comment) — a
    // small/weak model given raw marker text as the most prominent thing in
    // the prompt will happily parrot it back as the "title" despite being
    // told not to (confirmed real report: a title of literally "--- Pasted
    // text --- 130 words."). Stripping before the model ever sees it is the
    // actual fix; `sanitize_title` below is only a second line of defense
    // for whatever slips past that.
    let first =
        strip_leading_attachment_markers(&strip_prompt_wrappers(&row.content.unwrap_or_default()));
    let first = first.trim();
    if first.is_empty() {
        return None;
    }
    // A long first message is mostly pasted context; the ask is at the top
    // and the tail only dilutes the prompt for a small model.
    let first: String = first.chars().take(2000).collect();

    // Kept as one flat line per rule so the model sees no stray indentation
    // (a `\`-continued Rust string literal keeps every leading space of the
    // next source line, which is exactly the kind of noise a 300M-class
    // summarizer copies into its answer).
    let instructions = concat!(
        "Below is the first message a user sent in a new chat. ",
        "Write a title for that chat saying what the user is asking about.\n",
        "Rules: at most 5 words. No quotes, no trailing punctuation, no preamble. ",
        "Describe the subject, not how any file or pasted text arrived. ",
        "Never copy the message verbatim.\n\n",
        "--- message ---\n",
    );
    let prompt = vec![json!({
        "role": "user",
        "content": format!("{instructions}{first}"),
    })];

    let schema = json!({
        "type": "object",
        "properties": { "title": { "type": "string" } },
        "required": ["title"],
    });

    let result = summarizer
        .structured_chat_for_session(provider_id, model, prompt, &schema)
        .await
        .ok()?;
    let title = result.get("title")?.as_str()?.trim();
    if title.is_empty() {
        return None;
    }
    Some(sanitize_title(title))
}

/// Strip surrounding quotes a model sometimes wraps the title in despite the
/// schema, collapse internal whitespace, and cap length the same way the
/// naive fallback does.
fn sanitize_title(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches(|c| c == '"' || c == '\'').trim();
    // Second line of defense: `summarizer_title` now strips attachment
    // markers before the model ever sees them, but this still catches a
    // model that echoes one back anyway (or any other input path that
    // reaches `sanitize_title` without going through that stripping).
    let trimmed = strip_leading_attachment_markers(trimmed);
    let collapsed = truncate_title_words(&trimmed);
    truncate_title(&collapsed)
}

#[cfg(test)]
mod derive_title_tests {
    // Several expectations below end in `…`: `derive_title` caps every title
    // at five words (`MAX_TITLE_WORDS`), so a six-word message is clipped.
    // That is the cap, not the marker-stripping, doing the work — these
    // tests are still asserting what got stripped off the *front*.
    use super::{
        derive_title, sanitize_title, strip_leading_attachment_markers, truncate_title,
        truncate_title_words,
    };

    #[test]
    fn sanitize_title_caps_at_five_words() {
        assert_eq!(
            sanitize_title("Debugging a login redirect loop in staging"),
            "Debugging a login redirect loop…"
        );
    }

    #[test]
    fn sanitize_title_leaves_five_words_alone() {
        assert_eq!(
            sanitize_title("Debugging a login redirect loop"),
            "Debugging a login redirect loop"
        );
    }

    #[test]
    fn derive_title_caps_the_naive_fallback_at_five_words() {
        // The fallback used to hand back the whole first line up to 60
        // chars, which is what filled the sidebar with raw prompts like
        // "please write me a python script to…".
        let msg = "please write me a python script that counts words";
        assert_eq!(derive_title(msg), "please write me a python…");
    }

    #[test]
    fn truncate_title_words_is_char_safe_on_multi_byte_text() {
        // Word-splitting a CJK/emoji title must not panic or split a
        // grapheme — it only ever slices at whitespace boundaries.
        let out = truncate_title_words("日本語 🎉 テスト です ね よ");
        assert_eq!(out, "日本語 🎉 テスト です ね…");
    }

    #[test]
    fn sanitize_title_strips_surrounding_quotes() {
        assert_eq!(
            sanitize_title("\"Debugging a login redirect loop\""),
            "Debugging a login redirect loop"
        );
        assert_eq!(
            sanitize_title("'Planning a trip to Japan'"),
            "Planning a trip to Japan"
        );
    }

    #[test]
    fn sanitize_title_collapses_internal_whitespace() {
        assert_eq!(
            sanitize_title("Fixing   a   flaky\ntest"),
            "Fixing a flaky test"
        );
    }

    #[test]
    fn truncate_title_leaves_a_short_title_untouched() {
        assert_eq!(truncate_title("Short title"), "Short title");
    }

    #[test]
    fn truncate_title_breaks_on_a_word_boundary_past_60_chars() {
        let long = "a".repeat(55) + " overflow-word-that-pushes-past-the-limit";
        let out = truncate_title(&long);
        assert!(out.ends_with('…'));
        assert!(out.chars().count() <= 61); // 60 chars + the ellipsis
        assert!(!out.contains("overflow-word"));
    }

    #[test]
    fn truncate_title_is_char_safe_on_multi_byte_text() {
        // A CJK-heavy title straddling the 60-char cut point must not panic
        // ("byte index is not a char boundary" — the exact bug this
        // char-count-based truncation avoids).
        let long = "文".repeat(80);
        let out = truncate_title(&long);
        assert!(out.chars().count() <= 61);
    }

    #[test]
    fn strips_a_single_dropped_file_marker_with_nothing_typed() {
        let msg = "--- lec11-remediated.pdf ---\nfull file contents here";
        assert_eq!(strip_leading_attachment_markers(msg), "");
        // Nothing left to title from — derive_title must not surface the
        // raw marker (the exact bug reported: a title of literally
        // "--- lec11-remediated.pdf ---").
        assert_eq!(derive_title(msg), "");
    }

    #[test]
    fn strips_a_pasted_text_marker_with_nothing_typed() {
        let msg = "--- Pasted text — 83 words ---\nsome pasted content";
        assert_eq!(derive_title(msg), "");
    }

    #[test]
    fn keeps_the_users_own_text_after_a_marker_block() {
        let msg = "--- lec11-remediated.pdf ---\nfull file contents\n\nSummarize this for me";
        assert_eq!(derive_title(msg), "Summarize this for me");
    }

    #[test]
    fn skips_multiple_leading_marker_blocks() {
        let msg =
            "--- a.txt ---\ncontent a\n\n--- b.txt ---\ncontent b\n\nWhat do these have in common?";
        assert_eq!(derive_title(msg), "What do these have in…");
    }

    #[test]
    fn a_message_with_no_markers_is_unaffected() {
        let msg = "How do I center a div?";
        assert_eq!(derive_title(msg), "How do I center a…");
    }

    #[test]
    fn a_line_that_merely_starts_with_dashes_is_not_treated_as_a_marker() {
        // Only a *whole line* matching `--- ... ---` counts — a message that
        // happens to start with "---" for some other reason (a markdown
        // horizontal rule, a code fence) must not be swallowed.
        let msg = "--- this is not a marker\nbecause it has no closing dashes";
        assert_eq!(derive_title(msg), "--- this is not a…");
    }

    #[test]
    fn sanitize_title_strips_a_marker_the_model_echoed_back() {
        // Defense-in-depth (release-fixes-2): the primary fix is that
        // `summarizer_title` now strips markers from the model's *input*
        // (see its own doc comment), so this only needs to cover a model
        // that still echoes the exact marker line back verbatim as its
        // whole answer (the reported bug: a title of literally "--- Pasted
        // text --- 130 words."). A title response has no realistic reason
        // to carry the `\n\n`-separated block structure `derive_title`'s
        // fallback strips real message content against, so this is
        // deliberately narrower than that path.
        assert_eq!(sanitize_title("--- Pasted text — 130 words ---"), "");
    }
}

/// Core agent loop: manages LLM turns, tool execution, HITL, and compaction.
pub struct AgentLoop {
    /// Used only to resolve which app owns the session being run, and
    /// that app's default provider. Provider selection is per-app now, and
    /// the loop is the layer that knows the session id.
    pool: sqlx::SqlitePool,
    router: Arc<ProviderRouter>,
    hitl: Arc<Mutex<HITLManager>>,
    mcp: Arc<MCPManager>,
    /// Keyed by HITL `action_id`; woken by the `/approve` route (once it
    /// exists) via `record_decision` + `notify_one()` so a paused tool call
    /// can resume. Shared with whatever owns this loop (see Phase G's `Agent`).
    hitl_notifies: Arc<DashMap<String, Arc<Notify>>>,
    context: ContextBuilder,
    stats: SessionStats,
    summarizer: Arc<SummarizerChain>,
    summarizer_cfg: SummarizerConfig,
    /// Pre-flight recall config (enabled/bm25 gate/token budgets). Passed
    /// through to both `preflight_recall` and post-turn `run_compaction`.
    memory_cfg: MemoryConfig,
    /// Shared daemon-wide recall counters (see `Agent::preflight`).
    preflight: Arc<PreflightCounters>,
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
    /// See `sandbox::check_containment`'s `strict` parameter and
    /// `AgentConfig::sandbox_strict`.
    sandbox_strict: bool,
    /// Behavioral-memory engine. `None` when disabled.
    plugins: Arc<crate::plugins::PluginHost>,
    /// Pathway learning cadence (`learn_every_n` exchanges).
    pathway_cfg: PathwayConfig,
    /// Sessions already warned about a pinned-provider mismatch (the
    /// `ModelFailover` notice at step 0). Shared with the daemon-lifetime
    /// `Agent` — this loop is rebuilt per turn, so the memory of "we already
    /// told the user" must outlive it. An entry is removed the moment the
    /// pinned provider resolves again, so a *new* mismatch appearance
    /// re-warns.
    provider_mismatch_warned: Arc<DashMap<String, ()>>,
    workspace_snapshots: Arc<DashMap<String, (String, String)>>,
    background_tasks: Arc<DashMap<String, Vec<tokio::task::AbortHandle>>>,
    /// The tool names this turn may call, or `None` for "everything the
    /// owning app can see" — the ordinary chat case.
    ///
    /// Set from session metadata at the top of `run_inner`, which is safe
    /// because a loop is built fresh per turn (`Agent::build_loop`) and holds
    /// no session state between them. It is a field rather than an argument
    /// because the enforcement point is `execute_one_tool_call`, five frames
    /// below the only place that can read the session's metadata.
    tool_allow: Option<std::collections::HashSet<String>>,
    /// Whether a tool call that would need human approval is refused outright
    /// instead of pausing for one. Set per session; see `run_inner`.
    hitl_auto_reject: bool,
    /// How this turn's model calls queue for the provider. Interactive by
    /// default; a detached run (job, schedule, specialist) sets `Background` so
    /// a user waiting on a chat message never sits behind a batch.
    ///
    /// Was hardcoded `Interactive` at the one `chat_completion` site, which
    /// silently contradicted both `routes::jobs`' own comment and API.md — so
    /// every job competed with live chat at full priority.
    priority: crate::provider::queue::Priority,
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
        summarizer: Arc<SummarizerChain>,
        summarizer_cfg: SummarizerConfig,
        memory_cfg: MemoryConfig,
        preflight: Arc<PreflightCounters>,
        max_concurrent_tool_calls: usize,
        cache_dir: String,
        fallback_cfg: FallbackConfig,
        sandbox_strict: bool,
        plugins: Arc<crate::plugins::PluginHost>,
        pathway_cfg: PathwayConfig,
        provider_mismatch_warned: Arc<DashMap<String, ()>>,
        workspace_snapshots: Arc<DashMap<String, (String, String)>>,
        background_tasks: Arc<DashMap<String, Vec<tokio::task::AbortHandle>>>,
        pool: sqlx::SqlitePool,
    ) -> Self {
        Self {
            pool,
            router,
            hitl,
            mcp,
            hitl_notifies,
            context,
            stats,
            summarizer,
            summarizer_cfg,
            memory_cfg,
            preflight,
            max_concurrent_tool_calls,
            cache_dir,
            fallback_cfg,
            sandbox_strict,
            plugins,
            pathway_cfg,
            provider_mismatch_warned,
            workspace_snapshots,
            background_tasks,
            tool_allow: None,
            hitl_auto_reject: false,
            priority: crate::provider::queue::Priority::Interactive,
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
        priority: crate::provider::queue::Priority,
    ) {
        self.priority = priority;
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
        // `filter(|m| !m.is_empty())`: `activate_provider` stamps
        // `models.first().cloned().unwrap_or_default()`, so a profile with no
        // discovered models writes `"model": ""` into session metadata. Without
        // this filter that empty string is a *successful* override — it wins
        // over the provider's configured model and goes out as an empty
        // `model` field on the wire. An empty pin is no pin; treat it as absent
        // so the provider's own default applies.
        let model_override = metadata
            .get("model")
            .and_then(|v| v.as_str())
            .filter(|m| !m.trim().is_empty());

        let allowed_dirs = allowed_dirs_for_session(&metadata, &self.cache_dir);
        // Hand the same set to the bundled stdio tool servers.
        //
        // Those enforce a boundary of their own as defense-in-depth, and it is
        // resolved from their environment — which, for a process shared by
        // every session and spawned once, cannot know about this session's
        // attached files or chosen working folders. Without this the two gates
        // disagree: the daemon allows the session's chat directory, the tool
        // refuses it as outside its own home, and the model is left unable to
        // open a file the user just attached. See `mcp::kitty_grants`.
        if let (Some(app_id), false) = (
            crate::storage::sessions::owner_of(&self.pool, session_id)
                .await
                .ok()
                .flatten(),
            self.cache_dir.is_empty(),
        ) {
            crate::mcp::kitty_grants::publish(
                std::path::Path::new(&self.cache_dir),
                &app_id,
                &allowed_dirs,
            );
        }
        let chat_dir = metadata.get("chat_dir").and_then(|v| v.as_str());
        let cwd = metadata.get("cwd").and_then(|v| v.as_str());

        // Orientation block for the prompt head. Rebuilt only when this
        // session's working directory differs from the one the cached listing
        // describes — see `Agent::workspace_snapshots`.
        let workspace_snapshot: Option<String> = cwd.and_then(|dir| {
            if let Some(hit) = self.workspace_snapshots.get(session_id) {
                if hit.0 == dir {
                    return Some(hit.1.clone());
                }
            }
            let rendered = crate::agent::context::workspace_snapshot::block(dir)?;
            self.workspace_snapshots
                .insert(session_id.to_string(), (dir.to_string(), rendered.clone()));
            Some(rendered)
        });

        // Only this app's own MCP servers, plus the shared pool. An
        // unresolvable owner falls through to `""`, which matches no private
        // server and so yields the shared pool alone -- a scheduled run on an
        // unexpected session keeps working with shared tools rather than
        // either failing or seeing everyone's.
        let tool_scope = crate::storage::sessions::owner_of(&self.pool, session_id)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let mut active_tools: Vec<ToolDefinition> = self.mcp.list_tools_for_app(&tool_scope);

        // Per-run narrowing, on top of the per-app scope above. A specialist
        // gets only the tools its definition names; an ordinary chat turn has
        // no list and keeps everything.
        //
        // Filtering here shapes what the model is *offered*. It is not the
        // enforcement point — a model can name a tool it was never shown, so
        // `execute_one_tool_call` checks the same list again at dispatch. Both,
        // deliberately: the filter is what makes the restriction cheap (a
        // narrower prompt), the check is what makes it true.
        self.tool_allow = parse_tool_allow(&metadata);
        // A detached run — a specialist, a job, a scheduled turn — has no user
        // attached to answer an approval prompt. The bounded wait below is an
        // hour, which is not a hang but is indistinguishable from one to a
        // caller waiting on the result, and it is an hour *per tool call*. Such
        // a run refuses instead, immediately, and reports the refusal in its
        // own answer.
        self.hitl_auto_reject = metadata
            .get("hitl_policy")
            .and_then(|v| v.as_str())
            .is_some_and(|p| p == "auto_reject");
        if let Some(allow) = self.tool_allow.as_ref() {
            let before = active_tools.len();
            active_tools.retain(|t| allow.contains(&t.name) || t.name == BUDGET_TOOL);
            tracing::debug!(
                session_id,
                before,
                after = active_tools.len(),
                "tool set narrowed by the session's allow-list"
            );
        }
        let active_tools = active_tools;

        // The active provider's own `context_length` (Settings → Providers →
        // Advanced) wins over the daemon-wide `token_management.max_context_tokens`
        // default when set — same override `context_length` uses below for the
        // post-turn compaction check. `.ok()` here (rather than surfacing "no
        // healthy providers" as an error) is deliberate: this is a soft budget
        // hint for context assembly, not the point where an unresolvable
        // provider should abort the turn — `run_tool_loop`'s own
        // `get_provider_id` call does that properly a few lines of control flow
        // later. Resolved once and reused by the pathway-recall path-pick
        // below, which also needs to know which provider/model is active.
        let resolved_provider_id = self
            .resolve_provider(session_id, effective_provider.as_deref())
            .await
            .ok();
        let context_tokens_override = resolved_provider_id
            .as_deref()
            .and_then(|pid| self.router.context_length(pid));

        // Drop this session's *own* leftover turn-end work before starting a
        // new turn: a compaction or title call for the previous message is
        // superseded by the one about to run, and holding both would have two
        // summarizers writing the same session's metadata.
        //
        // Scoped to this session on purpose. V1 cancelled by provider, which
        // meant a new turn here aborted an unrelated session's compaction --
        // work that session had already paid for and would silently lose.
        // Freeing the endpoint from another session's background task is the
        // queue's job, via `Priority::Background`.
        self.cancel_background(session_id);

        // Adaptive Pathway turn-start hook: in-process recall so the model
        // sees learned behavioral beliefs *before* picking tools this turn.
        // Cache-aware by construction: exactly one of `ap_hints`/`thought_seed`
        // is ever `Some` (see `pathway_recall`'s doc comment) and each is
        // injected into the tail region — `ap_hints` right before the new
        // user message, `thought_seed` right after it — never into the
        // stable head, and a disabled engine (or a turn with no beliefs)
        // produces `(None, None)`, i.e. zero delta to the prompt (the
        // byte-identity regression test in `context/builder.rs` guards
        // this).
        let (ap_hints, thought_seed) = match &resolved_provider_id {
            Some(pid) => {
                let model = self.router.resolve_model(pid, model_override);
                self.pathway_recall(session_id, user_message, pid, &model)
                    .await
            }
            None => (None, None),
        };

        // Pre-flight memory recall ("the detour"): best-effort FTS5 lookup
        // over the session's *already-compacted* history, gated by recall
        // intent, injected into the tail region (like `ap_hints`) so the
        // stable head stays byte-identical. Any miss/disabled/error yields
        // `None` → zero delta to the prompt; the counter drops are the only
        // side effect.
        let preflight_recalled = self
            .preflight_recall(session_id, user_message, session.compacted_through_rowid)
            .await;
        let recalled = preflight_recalled.as_deref();

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
                workspace_snapshot.as_deref(),
                ap_hints.as_deref(),
                recalled,
                thought_seed.as_deref(),
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

        // The thought-seed is an ephemeral prefill for the provider's eyes
        // only — never transcript content. `build_messages` appended it as
        // the trailing assistant message; strip it back off BEFORE the first
        // persistence below, or the literal `<think>` seed lands in saved
        // chats (and the next turn's request would carry two adjacent
        // assistant messages — a 400 on Anthropic). `run_tool_loop` gets it
        // separately and appends it to the outgoing provider request only.
        let thought_seed_msg =
            crate::agent::context::builder::strip_trailing_thought_seed(&mut messages);

        // Session title derivation (release-fixes item 12) moved to
        // `derive_and_set_title`, fired post-turn from `run_tool_loop` once
        // there's an actual exchange for the summarizer to work from,
        // instead of here at send time off just the raw first message. Same
        // "only when not already named" gate, re-checked there.

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
            cwd,
            effective_provider,
            model_override,
            messages,
            &metadata,
            &active_tools,
            event_tx,
            thought_seed_msg,
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
    /// Remember a fire-and-forget task so a later turn on the same provider
    /// can abort it. Also drops handles that have already finished, so the
    /// vector cannot grow across a long session.
    /// Record turn-end background work (compaction, title derivation, the
    /// pathway learn pass) against the **session** that spawned it.
    ///
    /// V1 keyed this by provider, which was right when one client owned the
    /// daemon and wrong the moment two sessions shared an endpoint: starting a
    /// turn on session A would abort session B's in-flight compaction, so B
    /// silently lost work it had already paid for. Cross-session contention
    /// for the endpoint is the queue's job (see `provider::queue`), not
    /// something to resolve by cancelling someone else's turn-end work.
    fn track_background(&self, session_id: &str, handle: tokio::task::AbortHandle) {
        let mut entry = self
            .background_tasks
            .entry(session_id.to_string())
            .or_default();
        entry.retain(|h| !h.is_finished());
        entry.push(handle);
    }

    /// Abort any turn-end background work still running against this provider.
    ///
    /// Called at the top of a turn. Aborting drops the in-flight `reqwest`
    /// future, which closes the connection and lets a single-slot server free
    /// the slot immediately — rather than the user's message waiting out a
    /// summarization it never asked for. See `Agent::background_tasks` for why
    /// this loses nothing.
    fn cancel_background(&self, session_id: &str) {
        if let Some((_, handles)) = self.background_tasks.remove(session_id) {
            let live = handles.iter().filter(|h| !h.is_finished()).count();
            if live > 0 {
                tracing::debug!(
                    session_id,
                    live,
                    "aborting this session's own turn-end background work"
                );
            }
            for h in handles {
                h.abort();
            }
        }
    }

    /// Fire off a compaction pass for this session and return immediately.
    ///
    /// Extracted so the post-turn pass and the two context-overflow exits
    /// share one definition. The overflow exits are the reason this is not
    /// still inline: they `return` out of the tool loop from *above* the
    /// post-turn block, so the one pass that could shrink the session for the
    /// user's next message was skipped in exactly the situation that needed
    /// it — which is why a blown context stayed blown until the user found
    /// `/compact` by hand. Those callers pass `force = true`, since the
    /// high-water gate is about routine housekeeping and this is not that.
    ///
    /// Fire-and-forget in every case: the CAS lock inside `run_compaction` is
    /// the overlap guard if two passes for one session ever race, and a slow
    /// summarizer must never hold up the turn's terminal event.
    fn spawn_compaction(
        &self,
        pool: &SqlitePool,
        session_id: &str,
        provider_id: &str,
        model: Option<String>,
        context_length: i32,
        force: bool,
    ) {
        let pool = pool.clone();
        let session_id = session_id.to_string();
        // Kept back from the move below so the spawned task can still be
        // registered against the session that owns it.
        let tracked_session = session_id.clone();
        let provider_id = provider_id.to_string();
        let summarizer = self.summarizer.clone();
        let token_cfg = self.context.config().clone();
        let summarizer_cfg = self.summarizer_cfg.clone();
        let memory_cfg = self.memory_cfg.clone();
        let handle = tokio::spawn(async move {
            let outcome = run_compaction(
                &pool,
                &session_id,
                &summarizer,
                Some(provider_id.as_str()),
                model,
                &token_cfg,
                &summarizer_cfg,
                &memory_cfg,
                context_length,
                force,
            )
            .await;
            // Still fire-and-forget as far as the turn is concerned, but no
            // longer silent. A forced pass runs because a turn just overflowed
            // the window and the user was told the session "is being compacted
            // — send your message again"; when that pass frees nothing, the
            // same overflow repeats on every retry. Logging the reason at
            // `warn` is what makes that diagnosable at all, since the promise
            // itself is made on a stream this task cannot reach.
            match outcome {
                Ok(r) => tracing::debug!(
                    "compaction: session {session_id} folded {} messages ({} -> {} tokens)",
                    r.messages_compacted,
                    r.tokens_before,
                    r.tokens_after,
                ),
                Err(skip) if force => tracing::warn!(
                    "compaction: forced pass for session {session_id} freed nothing: {}",
                    skip.message(),
                ),
                Err(skip) => tracing::debug!(
                    "compaction: session {session_id} skipped: {}",
                    skip.message(),
                ),
            }
        });
        self.track_background(&tracked_session, handle.abort_handle());
    }

    #[allow(clippy::too_many_arguments)]
    /// Which app owns `session_id`, and what that app's default provider is.
    ///
    /// Both are `None` for a session with no resolvable owner, which should not
    /// happen -- ownership is stamped at creation -- but must degrade to V1's
    /// global behaviour rather than killing the turn, since a background or
    /// scheduled run reaching here with an unexpected session is not worth
    /// failing a user's work over.
    async fn app_scope(&self, session_id: &str) -> (Option<String>, Option<String>) {
        let Ok(Some(app_id)) = crate::storage::sessions::owner_of(&self.pool, session_id).await
        else {
            return (None, None);
        };
        let default = crate::storage::apps::get_app(&self.pool, &app_id)
            .await
            .ok()
            .flatten()
            .and_then(|a| a.default_provider_id);
        (Some(app_id), default)
    }

    /// Resolve a provider for this turn, scoped to the session's owning app.
    ///
    /// Falls back to the daemon-global selection only when the session has no
    /// resolvable owner -- see [`Self::app_scope`].
    /// A model pin is only valid for the provider it was pinned to.
    ///
    /// `model_override` comes from the session's `model` metadata and was
    /// chosen *for* `pinned_provider`. When the resolved provider is a
    /// substitute — the pinned one was deleted, or a mid-turn failover moved
    /// us — carrying the override across sends one vendor's model id to
    /// another's endpoint. Best case that is a 400; worst case a self-hosted
    /// server ignores the field and serves whatever it has loaded, which looks
    /// exactly like success.
    fn model_for(
        &self,
        resolved_provider: &str,
        pinned_provider: Option<&str>,
        model_override: Option<&str>,
    ) -> String {
        let pin_still_applies = pinned_provider == Some(resolved_provider);
        self.router.resolve_model(
            resolved_provider,
            pin_still_applies.then_some(model_override).flatten(),
        )
    }

    async fn resolve_provider(
        &self,
        session_id: &str,
        preferred: Option<&str>,
    ) -> Result<String, crate::error::ProviderError> {
        match self.app_scope(session_id).await {
            (Some(app_id), default) => {
                self.router
                    .resolve_provider_for_app(&app_id, preferred, default.as_deref())
            }
            (None, _) => self.router.get_provider_id(preferred),
        }
    }

    /// Publish a validated structured answer as the turn's final message.
    ///
    /// Persisted as the last assistant message so
    /// `sessions::last_assistant_text` — what jobs and specialist runs read
    /// back — returns the validated JSON rather than the prose that preceded
    /// it. Shared by the live and cached paths so a cache hit is
    /// indistinguishable downstream.
    async fn emit_structured_answer(
        &mut self,
        session_id: &str,
        messages: &mut Vec<Value>,
        value: &Value,
        event_tx: &mpsc::UnboundedSender<SSEEvent>,
    ) {
        let rendered = value.to_string();
        let _ = event_tx.send(SSEEvent {
            event_type: SSEEventType::LlmDelta,
            content: Some(rendered.clone()),
            session_id: Some(session_id.to_string()),
            ..Default::default()
        });
        messages.push(json!({"role": "assistant", "content": rendered}));
        if let Err(e) = self.context.save_messages(session_id, messages).await {
            tracing::warn!("failed to save structured answer for session {session_id}: {e}");
        }
    }

    /// Re-ask for the turn's final answer with a schema attached, and
    /// validate it.
    ///
    /// The one retry is the whole point: a model that returns the wrong shape
    /// almost always fixes it when shown the validator's complaint, and a
    /// second failure is a signal the caller needs (a bad schema, or a model
    /// too small for it) rather than something to paper over. Unvalidated
    /// prose is never returned as if it were structured — a parent that asked
    /// for a guarantee gets one or gets an error.
    #[allow(clippy::too_many_arguments)]
    async fn finalize_structured(
        &mut self,
        session_id: &str,
        messages: &mut Vec<Value>,
        provider_id: &str,
        provider_model: &str,
        sampling: crate::provider::base::SamplingParams,
        app_id: &str,
        spec: &ResponseSpec,
        report_reserve: i32,
        cache: &crate::provider::response_cache::CacheDirective,
        event_tx: &mpsc::UnboundedSender<SSEEvent>,
    ) {
        const INSTRUCTION: &str = "Now return your final answer for this task,              as JSON matching the required schema. Return only the JSON.";

        // Thinking off, output room pinned — and this is a fix, not a
        // precaution.
        //
        // This request deliberately withholds tools, and on Anthropic that is
        // exactly the condition that switches extended thinking *on*:
        // `anthropic_thinking` short-circuits with
        // `if has_tools { return (max, None) }`, and that guard, derived from
        // the tool list, is the only thing suppressing thinking on a normal
        // agent step. The wrap-up valve documents this trap and defends against
        // it; this path inherited the caller's sampling untouched, which made
        // the report request the single most likely place in a delegate's whole
        // run for an unbudgeted think — after its budget had already been spent.
        //
        // `max_tokens` is pinned for the same reason the wrap-up path pins it:
        // zeroing the effort drops out of the `(base + 4096)` branch back to a
        // bare 4096, shrinking the answer's room at exactly the moment the
        // answer is due.
        let mut sampling = sampling;
        sampling.effort = None;
        sampling.reasoning_max_tokens = None;
        sampling.max_tokens = Some(
            sampling
                .max_tokens
                .unwrap_or(0)
                .max(report_reserve.max(REPORT_RESERVE_FLOOR)),
        );

        // Built from the turn's own history so the answer is grounded in the
        // tool results the loop actually gathered, not re-derived.
        let mut attempt: Vec<Value> = messages.clone();
        attempt.push(json!({"role": "system", "content": INSTRUCTION}));

        // Cached on the *final* request only, which is the one place in a
        // delegate's run where caching is unambiguously safe: it carries no
        // tools by construction, so replaying it cannot claim work that never
        // happened. It is also the expensive request to repeat and the one whose
        // inputs are most stable — same schema, same pinned model, same corpus.
        //
        // A hit takes no provider permit. That is the point: a hit that queued
        // behind live traffic would save the tokens but not the latency, which
        // is most of what a pipeline is buying.
        let cache_key = cache.is_active().then(|| {
            crate::provider::response_cache::cache_key(
                app_id,
                cache.shared,
                provider_id,
                provider_model,
                &attempt,
                None,
                &sampling_fingerprint(&sampling),
                spec.schema.as_ref(),
            )
        });

        if cache.read {
            if let Some(key) = cache_key.as_deref() {
                if let Ok(Some(hit)) = crate::provider::response_cache::get(&self.pool, key).await {
                    if let Some(value) = response_schema::extract(&hit) {
                        // Re-validated rather than trusted: an entry written
                        // under an older schema would otherwise be served as if
                        // it still matched.
                        let still_valid = spec
                            .schema
                            .as_ref()
                            .is_none_or(|sc| response_schema::validate(sc, &value).is_ok());
                        if still_valid {
                            tracing::debug!(session_id, "structured answer served from cache");
                            self.emit_structured_answer(session_id, messages, &value, event_tx)
                                .await;
                            return;
                        }
                    }
                }
            }
        }

        let mut last_error = String::new();
        for pass in 0..2 {
            let stream = match self
                .router
                .chat_completion(
                    provider_id,
                    &attempt,
                    // Withheld: this request is the answer, not another step.
                    None,
                    sampling.clone(),
                    Some(provider_model.to_string()),
                    None,
                    app_id,
                    self.priority,
                    spec,
                )
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    last_error = format!("structured-response request failed: {e}");
                    break;
                }
            };

            let (text, forced) = drain_structured_answer(stream).await;
            // Anthropic returns the answer as the forced tool's arguments;
            // every other dialect returns it as the message body.
            let candidate = forced.or_else(|| response_schema::extract(&text));
            let Some(value) = candidate else {
                last_error = "model returned no JSON for a schema-constrained answer".to_string();
                attempt.push(json!({"role": "assistant", "content": text}));
                attempt.push(json!({"role": "system", "content": format!(
                    "{last_error}. {INSTRUCTION}"
                )}));
                continue;
            };

            if let Some(schema) = spec.schema.as_ref() {
                if let Err(why) = response_schema::validate(schema, &value) {
                    last_error = format!("response did not match the required schema: {why}");
                    if pass == 0 {
                        tracing::info!(
                            session_id,
                            "structured answer failed validation; retrying once with the error"
                        );
                        attempt.push(json!({"role": "assistant", "content": value.to_string()}));
                        attempt.push(json!({"role": "system", "content": format!(
                            "That response was rejected — {why}. {INSTRUCTION}"
                        )}));
                    }
                    continue;
                }
            }

            if cache.write {
                if let Some(key) = cache_key.as_deref() {
                    let ttl = cache
                        .ttl_s
                        .unwrap_or(crate::provider::response_cache::DEFAULT_TTL_SECS);
                    if let Err(e) = crate::provider::response_cache::put(
                        &self.pool,
                        key,
                        &value.to_string(),
                        ttl,
                    )
                    .await
                    {
                        // Never fatal: a cache that cannot be written is slower,
                        // not wrong.
                        tracing::warn!("could not cache structured answer: {e}");
                    }
                }
            }
            self.emit_structured_answer(session_id, messages, &value, event_tx)
                .await;
            return;
        }

        tracing::warn!(session_id, error = %last_error, "structured answer unavailable");
        let _ = event_tx.send(SSEEvent {
            event_type: SSEEventType::Error,
            content: Some(last_error.clone()),
            error_message: Some(last_error),
            session_id: Some(session_id.to_string()),
            is_last: true,
            ..Default::default()
        });
    }

    async fn run_tool_loop(
        &mut self,
        session_id: &str,
        pool: &SqlitePool,
        allowed_dirs: &[String],
        // The session's working directory — the base a relative tool `path`
        // argument is qualified against (see `qualify_relative_path_args`).
        session_cwd: Option<&str>,
        effective_provider: Option<String>,
        model_override: Option<&str>,
        mut messages: Vec<Value>,
        metadata: &Value,
        active_tools: &[ToolDefinition],
        event_tx: &mpsc::UnboundedSender<SSEEvent>,
        thought_seed_msg: Option<Value>,
    ) {
        // `max_steps` is compared as i64, not truncated to i32, and clamped
        // to `1..=MAX_STEPS_CEILING` — the old `... as i32` truncated a huge
        // value, and a non-positive value made `step >= max_steps` instantly
        // true, ending the turn before any model call.
        let mut max_steps: i64 = metadata
            .get("max_steps")
            .and_then(|v| v.as_i64())
            .unwrap_or(50)
            .clamp(1, MAX_STEPS_CEILING);
        // Converted once per turn, not once per tool-loop iteration. The
        // registered tool set cannot change mid-turn, but
        // `tools_to_openai_format` clones every tool's `input_schema` and then
        // walks it recursively (`sanitize_boolean_subschemas`) — with ~40 tools
        // registered that was a full rebuild of the whole schema array on every
        // step. Iterations still take an owned copy, since the wrap-up and
        // step-nudge branches mutate their own view of it, but a `Vec<Value>`
        // clone is far cheaper than redoing the conversion.
        let tools_base: Vec<Value> = tools_to_openai_format(active_tools);
        // Resolved once per turn rather than per attempt: an unknown name
        // yields `None` (no preset), never someone else's settings.
        let preset = metadata
            .get("sampling_preset")
            .and_then(|v| v.as_str())
            .and_then(crate::provider::presets::resolve);
        // Requested reasoning effort for this session — translated per dialect
        // at each provider's wire boundary, ignored by dialects that have no
        // such parameter. Resolved once per turn like `preset`.
        let effort = metadata
            .get("thinking_effort")
            .and_then(|v| v.as_str())
            .and_then(crate::provider::base::Effort::from_wire);
        // Whether this turn's final answer may be served from, and written to,
        // the response cache.
        //
        // On for delegates and jobs, off for interactive chat: a repeat question
        // in a conversation usually wants a fresh answer, while a pipeline
        // re-running over the same corpus is paying for identical calls.
        let cache_directive: crate::provider::response_cache::CacheDirective = metadata
            .get("response_cache")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        // How much of this run may go on reasoning. Set by a specialist run;
        // absent for an ordinary chat turn, which is deliberately uncapped —
        // cutting off a user's own hard question is a worse failure than an
        // expensive one, and unlike a delegate they can see it happening and
        // stop it themselves.
        let reasoning_cap: Option<tokens::ReasoningCap> = metadata
            .get("reasoning_cap")
            .and_then(|v| serde_json::from_value(v.clone()).ok());

        // Models this session may never run on. Written into a delegate's
        // metadata by `Orchestrator::run`, absent for an ordinary chat turn.
        //
        // `subagent_pick::choose_host` gates the models it can see, but it is
        // not the last word: the provider is re-resolved here at step 0 (the
        // picked one may have gone unhealthy in between) and again on every
        // failover in the attempt loop, and neither of those knows a specialist
        // is asking. A delegate placed on a permitted host could therefore fail
        // over onto the expensive model the denylist exists to keep it off —
        // the same class of bypass as applying the list only while scoring.
        let model_deny: Vec<String> = metadata
            .get("subagent_model_deny")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        // The shape this turn's final answer must take. Set by a specialist
        // run (and by `POST /api/jobs` with a schema); absent, and therefore
        // `Text`, for an ordinary chat turn.
        let response_spec: ResponseSpec = metadata
            .get("response_schema")
            .cloned()
            .map(ResponseSpec::schema)
            .unwrap_or_default();
        let mut step: i64 = 0;
        // Wrap-up valve state, alongside the other survives-iterations values
        // below. `wrapup_issued` is a belt against re-injecting on a later
        // iteration; the unconditional `break` in the completion block is the
        // braces. Both, deliberately — see that block's comment.
        let mut wrapup_issued = false;
        // Reasoning spend across the whole run, and the latch it trips.
        //
        // Per *run*, not per response: `MAX_TURN_CONTENT_CHARS` already bounds
        // one response, and a delegate with twenty steps could spend that
        // twenty times over while never tripping it once.
        let mut reasoning_spent: i32 = 0;
        let mut reasoning_exhausted = false;
        // Derived from the answer schema: the room the delegate still needs
        // after thinking, to actually write its report.
        let report_reserve = response_spec
            .schema
            .as_ref()
            .map(|s| {
                (tokens::count_text_tokens(&s.to_string()) * 3)
                    .clamp(REPORT_RESERVE_FLOOR, REPORT_RESERVE_CEILING)
            })
            .unwrap_or(REPORT_RESERVE_FLOOR);
        // (messages.len(), provider-reported input_tokens) as of the last
        // completed response. The provider's own count already includes the
        // tool schemas and system framing that a local count of `messages`
        // cannot see, so this is the accurate base and everything appended
        // since is the delta (`tokens::projected_input_tokens`).
        let mut last_usage: Option<(usize, i32)> = None;
        // The provider/model the last completed model call actually used
        // (fallback can switch mid-turn) — remembered for the ONCE-per-turn
        // post-turn compaction pass below.
        let mut last_provider_id: Option<String> = None;
        let mut last_provider_model: Option<String> = None;

        // Provider and model are resolved ONCE for the whole turn, not per
        // step.
        //
        // They used to be re-resolved from the live router on every tool-loop
        // iteration. The router is shared, and a Settings edit replaces the
        // entry in place: `sync_active_provider` PATCHes the profile, the
        // daemon's `update_provider` calls `register_from_row`, and that
        // `insert`s a fresh `ProviderEntry` over the one the in-flight turn is
        // using. So changing the default model in Settings switched the model
        // of every *running* turn at its next step — mid-thought, on the wire,
        // with no event and no banner (the mismatch warning below is gated on
        // `step == 0`). A chat must keep the model it started with.
        //
        // Retry/failover inside the attempt loop still reassigns both, and
        // that reassignment now persists for the rest of the turn rather than
        // being reverted by the next iteration's re-resolution — which is what
        // you want after failing over: a turn should not flap between engines
        // step by step.
        // Resolved once per turn, alongside the provider: every request this
        // turn makes queues in its owning app's lane, so one app's batch cannot
        // put another app's user behind it.
        let turn_app_id = self.app_scope(session_id).await.0;

        let mut turn_provider_id = match self
            .resolve_provider(session_id, effective_provider.as_deref())
            .await
        {
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
        let mut turn_provider_model = self.model_for(
            &turn_provider_id,
            effective_provider.as_deref(),
            model_override,
        );

        // Refused rather than downgraded. There is no cheaper host to fall back
        // to at this point — the picker already searched — and running anyway is
        // the one outcome the setting was added to prevent, so the honest answer
        // is to say which pattern stopped it and let the delegate report that.
        if let Some(pattern) =
            crate::agent::subagent_pick::denied_by(&turn_provider_model, &model_deny)
        {
            let msg = format!(
                "'{turn_provider_model}' matches '{pattern}' on the subagent denylist, so this \
                 specialist did not run. Edit the list in Settings, or give the specialist a \
                 provider that is not denied."
            );
            let _ = event_tx.send(SSEEvent {
                event_type: SSEEventType::Error,
                content: Some(msg.clone()),
                error_message: Some(msg),
                session_id: Some(session_id.to_string()),
                is_last: true,
                ..Default::default()
            });
            return;
        }

        loop {
            // Stop generating once the SSE consumer is gone — a disconnected
            // client's stream body is dropped by axum, the receiver end is
            // closed, and each further LLM round trip would be pure wasted
            // work. (The `disconnect_grace_secs` watcher in `Agent::run_turn`
            // is the backstop that aborts the whole task shortly after.)
            if event_tx.is_closed() {
                break;
            }

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

            let provider_id = turn_provider_id.clone();

            // The session pinned a provider that isn't registered, so the
            // router fell back to a different one. Tell the user once per
            // mismatch *appearance* (tracked daemon-side, since this loop is
            // rebuilt per turn) rather than silently running the whole
            // conversation on an engine they didn't choose — this is what
            // hid a bad provider stamp behind a working-looking chat on the
            // local engine. The entry clears the moment the pinned provider
            // resolves again, so a later re-occurrence re-warns.
            if step == 0 {
                match effective_provider.as_deref() {
                    Some(pinned) if pinned != provider_id => {
                        if self
                            .provider_mismatch_warned
                            .insert(session_id.to_string(), ())
                            .is_none()
                        {
                            let _ = event_tx.send(SSEEvent {
                                event_type: SSEEventType::ModelFailover,
                                content: Some(format!(
                                    "The provider this chat was set to ('{pinned}') isn't available — using '{provider_id}' instead. Re-pick the provider in settings if this isn't what you want."
                                )),
                                session_id: Some(session_id.to_string()),
                                ..Default::default()
                            });
                        }
                    }
                    _ => {
                        self.provider_mismatch_warned.remove(session_id);
                    }
                }
            }

            // A provider that can't take tools gets none — and the user is
            // told once per turn, rather than the old arrangement where the
            // provider silently dropped them behind a daemon-side `warn!` and
            // the session just looked like a model that refused to act.
            let provider_takes_tools = self.router.supports_tools(&provider_id);
            if !provider_takes_tools && !active_tools.is_empty() {
                let _ = event_tx.send(SSEEvent {
                    event_type: SSEEventType::ModelFailover,
                    content: Some(format!(
                        "Provider '{}' can't call tools — this turn runs without the {} tool(s) that are connected.",
                        provider_id,
                        active_tools.len()
                    )),
                    session_id: Some(session_id.to_string()),
                    ..Default::default()
                });
            }
            // Progressive budget check
            let mut in_budget_check = false;
            let mut in_wrapup = false;
            // Same two outcomes as the `provider_takes_tools` slice shadowing
            // this replaced: the full converted set, or nothing at all.
            let mut tools_for_turn: Vec<Value> = if provider_takes_tools {
                tools_base.clone()
            } else {
                Vec::new()
            };

            // How much room is left before the provider's own context limit.
            //
            // This is the in-loop check that used to not exist: context was
            // assembled once, before the loop, and then every iteration
            // appended tool results and re-sent the whole grown history with
            // nothing watching. A turn could start comfortably inside the
            // window and walk to 100% across 50 steps. Full compaction per
            // iteration was rightly removed for cost (see the post-turn pass
            // below); this is arithmetic on a running count, not a compaction
            // pass, so it costs effectively nothing.
            //
            // Resolved against the *pre-failover* provider, matching
            // `supports_tools` above. If the retry block switches provider
            // mid-attempt the window may differ; that inconsistency predates
            // this code and is not worth diverging from the neighbouring
            // pattern to fix here.
            let context_length = self
                .router
                .context_length(&provider_id)
                .unwrap_or(self.context.config().max_context_tokens);
            let token_cfg = self.context.config();
            let wrapup_reserve = tokens::context_reserve_tokens(
                context_length,
                token_cfg.wrapup_reserve_ratio,
                token_cfg.wrapup_reserve_cap,
            );
            let projected_input = tokens::projected_input_tokens(last_usage, &messages);
            let wrapup_due = tokens::wrapup_due(projected_input, context_length, wrapup_reserve);

            // Exactly one intervention per iteration, chosen here rather than
            // by two independent `if`s — see `decide_turn_mode`.
            match decide_turn_mode(step, wrapup_issued, wrapup_due) {
                TurnMode::WrapUp => {
                    if step == 0 {
                        // The fingerprint of a provider whose `context_length`
                        // is unset or wrong: the context builder assembled
                        // against the daemon-wide budget and blew the real
                        // window before a single tool ran. Without this line it
                        // looks like a mysteriously terse assistant.
                        tracing::warn!(
                            session_id,
                            context_length,
                            projected_input,
                            wrapup_reserve,
                            "wrap-up valve fired before any tool ran — check this \
                             provider's context_length"
                        );
                    }
                    tracing::info!(
                        session_id,
                        step,
                        context_length,
                        projected_input,
                        wrapup_reserve,
                        "context reserve reached — withdrawing tools for a wrap-up reply"
                    );
                    messages.push(json!({
                        "role": "system",
                        "content": WRAPUP_SYSTEM_MESSAGE
                    }));
                    in_wrapup = true;
                    wrapup_issued = true;
                    tools_for_turn.clear();
                    // Surfaced, unlike `__budget__` — the turn is about to end
                    // early and the user is owed the reason.
                    let _ = event_tx.send(SSEEvent {
                        event_type: SSEEventType::ToolFinish,
                        tool_name: Some(CONTEXT_BUDGET_TOOL.into()),
                        tool_result: Some(format!(
                            "Close to this model's context limit ({projected_input} of \
                             {context_length} tokens used) — finishing this turn now. \
                             Send another message to continue; the conversation will be \
                             compacted first."
                        )),
                        session_id: Some(session_id.to_string()),
                        ..Default::default()
                    });
                }
                // Fire the budget nudge at 20/40/60 *executed steps*. The old
                // check counted messages carrying `tool_calls`/`tool_call_id`,
                // which jumps by the number of tool calls per turn (usually > 1,
                // often several) — so it skipped over multiples of 20 entirely
                // and the nudge silently never fired for sessions doing any
                // parallel tool execution. `step` is incremented exactly once per
                // completed tool-loop iteration, so `step % 20 == 0` lands on the
                // 20th, 40th, 60th... iteration reliably.
                TurnMode::StepNudge => {
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
                TurnMode::Normal => {}
            }

            // ----------------------------------------------------------------
            // Pre-flight: never hand the provider a request we already know is
            // too big.
            //
            // The wrap-up valve above can only stop the history *growing* — it
            // withdraws tools and clamps `max_tokens`, then sends the same
            // array one more time. When a single step's tool results have
            // already carried the history past the window (the results cap is
            // 100 KB each, ~25-33k tokens, against a 15k default reserve, so
            // one step can clear the guard in a single bound) that wrap-up
            // request is itself the one that 400s, and the turn dies
            // mid-thought with nothing salvaged. This is the check that was
            // missing entirely: measure, shrink, and only then send.
            // ----------------------------------------------------------------
            {
                // The delta estimate is deliberately cheap; a full cl100k
                // encode of the transcript per iteration is what
                // `projected_input_tokens` exists to avoid. Only pay for the
                // exact count once the estimate says we are anywhere near the
                // edge.
                let preflight_gate =
                    context_length.saturating_sub(wrapup_reserve.saturating_mul(3) / 2);
                if projected_input > preflight_gate {
                    // Tool schemas ride along on every request and are
                    // routinely 2-6k tokens, so a count of `messages` alone
                    // understates what the provider will bill.
                    let schema_tokens = if in_wrapup {
                        0
                    } else {
                        tokens::count_text_tokens(
                            &serde_json::to_string(&tools_for_turn).unwrap_or_default(),
                        )
                    };
                    let budget = context_length
                        .saturating_sub(PREFLIGHT_OUTPUT_FLOOR)
                        .saturating_sub(schema_tokens);

                    if tokens::count_messages_tokens(&messages) > budget {
                        if let Some(shrunk) =
                            crate::agent::compaction::shrink_live_turn(&messages, budget, token_cfg)
                        {
                            let before = tokens::count_messages_tokens(&messages);
                            let after = tokens::count_messages_tokens(&shrunk);
                            tracing::info!(
                                session_id,
                                step,
                                context_length,
                                before,
                                after,
                                "pre-flight shrink: elided in-turn history to fit the window"
                            );
                            messages = shrunk;
                            // The user is owed the reason their earlier tool
                            // output stopped being visible to the model.
                            let _ = event_tx.send(SSEEvent {
                                event_type: SSEEventType::ToolFinish,
                                tool_name: Some(CONTEXT_BUDGET_TOOL.into()),
                                tool_result: Some(format!(
                                    "Trimmed earlier steps of this turn to stay inside \
                                     this model's context window ({before} → {after} of \
                                     {context_length} tokens). Continuing."
                                )),
                                session_id: Some(session_id.to_string()),
                                ..Default::default()
                            });
                            // The provider's count no longer describes this
                            // array — drop the mark so the next iteration
                            // recounts from scratch instead of adding a delta
                            // to a base that included what we just removed.
                            last_usage = None;
                        }
                    }

                    // Still over after shrinking: stop asking for tools and
                    // take the smallest possible final reply. This is the same
                    // shape the wrap-up valve produces, reached from the other
                    // direction.
                    if !in_wrapup && tokens::count_messages_tokens(&messages) > budget {
                        // `decide_turn_mode` guarantees exactly one injected
                        // system message per iteration, and both branches rely
                        // on popping *the last* message to remove their own.
                        // Overriding a step nudge from here would push a
                        // second one and leave both `in_budget_check` and
                        // `in_wrapup` set, so retract the nudge first — running
                        // out of context outranks running out of steps, and the
                        // nudge would be asking the model to keep going into a
                        // window that has no room left anyway.
                        if in_budget_check {
                            messages.pop();
                            in_budget_check = false;
                        }
                        messages.push(json!({
                            "role": "system",
                            "content": WRAPUP_SYSTEM_MESSAGE
                        }));
                        in_wrapup = true;
                        wrapup_issued = true;
                        tools_for_turn.clear();
                    }

                    // Nothing left to give. Fail here, deliberately, rather
                    // than letting the provider fail: this way the error is
                    // correctly classified, compaction runs before the user's
                    // next send, and the session does not become a wall the
                    // user can only escape with a manual `/compact`.
                    let final_count = tokens::count_messages_tokens(&messages);
                    if final_count > context_length {
                        tracing::warn!(
                            session_id,
                            step,
                            context_length,
                            final_count,
                            "pre-flight: cannot fit this turn in the window even after \
                             shrinking — ending the turn and compacting"
                        );
                        self.spawn_compaction(
                            pool,
                            session_id,
                            &provider_id,
                            Some(provider_model_for(
                                &self.router,
                                &provider_id,
                                model_override,
                            )),
                            context_length,
                            true,
                        );
                        let _ = event_tx.send(SSEEvent {
                            event_type: SSEEventType::ProviderError,
                            error_message: Some(format!(
                                "This conversation no longer fits in {model_label}'s \
                                 {context_length}-token context window ({final_count} tokens \
                                 needed). It is being compacted — send your message again.",
                                model_label =
                                    provider_model_for(&self.router, &provider_id, model_override),
                            )),
                            error_type: Some("context_exceeded".into()),
                            session_id: Some(session_id.to_string()),
                            is_last: true,
                            ..Default::default()
                        });
                        return;
                    }
                }
            }

            let mut provider_id = provider_id;
            let mut provider_model = turn_provider_model.clone();

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
            let turn_result = loop {
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
                //
                // A session's `sampling_preset` (§6.2/D6) merges *over* that
                // floor, so a preset overrides only what it names and the
                // per-dialect floor still fills the rest.
                let provider_sampling = self.router.sampling(&provider_id);
                let mut sampling = match preset.as_ref() {
                    Some(p) => crate::provider::sampling::merge(p, &provider_sampling),
                    None => provider_sampling,
                };
                // Applied after the preset/floor merge — effort is a level to
                // translate per dialect, not a knob to merge, and neither
                // presets nor floors carry one to combine it with. Cloned
                // because this runs once per tool-loop iteration and `Effort` is
                // no longer `Copy` (it can carry a model-specific level string).
                sampling.effort = effort.clone();
                if let Some(cap) = reasoning_cap {
                    if reasoning_exhausted {
                        // Latched. Zero the effort *and* pin an explicit
                        // max_tokens: on Anthropic, dropping the effort alone
                        // falls out of the `(base + 4096)` branch back to a bare
                        // 4096, which would shrink the delegate's output room at
                        // exactly the step it has to write its report.
                        sampling.effort = None;
                        sampling.reasoning_max_tokens = None;
                        sampling.max_tokens = Some(
                            sampling
                                .max_tokens
                                .unwrap_or(0)
                                .max(report_reserve.max(REPORT_RESERVE_FLOOR)),
                        );
                    } else {
                        // A hint, where the dialect has a field for it. The
                        // accumulator below is what actually enforces this.
                        let remaining = (tokens::reasoning_budget_tokens(
                            cap,
                            Some(context_length),
                            projected_input,
                            report_reserve,
                            i32::MAX,
                        ) - reasoning_spent)
                            .max(0);
                        sampling.reasoning_max_tokens = Some(remaining);
                    }
                }
                if in_wrapup {
                    // Counter-intuitive and load-bearing: withdrawing the tools
                    // switches Anthropic extended thinking *on*.
                    // `anthropic_thinking` short-circuits with
                    // `if has_tools { return (max, None) }`, and that guard —
                    // derived from the very tool list emptied above — is the
                    // only thing suppressing thinking on a normal agent step.
                    // At Medium effort with no explicit cap it would otherwise
                    // return `((16384 + 4096).min(65536), Some(16384))`, i.e.
                    // max_tokens jumping 4096 -> 20480 on the one request
                    // issued *because* input is already near the ceiling.
                    //
                    // Clamping max_tokens alone does not fix it: with a 2048
                    // cap the budget arithmetic yields `16384.min(2048 - 1024)`
                    // = 1024, which clears the >= 1024 test, so thinking stays
                    // on and eats half the wrap-up budget on reasoning nobody
                    // will read. Zero the effort as well.
                    sampling.effort = None;
                    sampling.max_tokens = Some(tokens::wrapup_max_tokens(
                        context_length,
                        projected_input,
                        WRAPUP_MAX_TOKENS_FLOOR,
                        WRAPUP_MAX_TOKENS_CEILING,
                    ));
                }
                // The ephemeral thought-seed prefill rides the FIRST provider
                // request only: it primes the model's reply to the new user
                // message; later iterations continue from real tool results.
                // It is deliberately NOT in `messages` (so it can never be
                // persisted into the transcript) — appended to a copy here
                // instead.
                //
                // That copy is now made *only* when there is a seed to append.
                // This used to deep-clone the entire conversation on every
                // attempt, inside the retry loop, inside the step loop — N
                // steps x M attempts full copies of a structure that grows all
                // turn, and with serde_json's `preserve_order` every clone
                // rebuilds an IndexMap per message. The providers immediately
                // rebuild the array anyway (system-message merge/hoist, tool
                // argument stringification), so the caller's copy was pure
                // duplicate work; they take `&[Value]` now and materialize once.
                let seeded: Option<Vec<Value>> = match &thought_seed_msg {
                    Some(seed) if step == 0 => {
                        let mut o = messages.clone();
                        o.push(seed.clone());
                        Some(o)
                    }
                    _ => None,
                };
                let outgoing: &[Value] = seeded.as_deref().unwrap_or(&messages);
                // A `chat_completion` `Err` and a mid-stream failure from
                // `process_stream` are the same kind of failure — a transient
                // error that the retry/failover block below must handle with a
                // shared attempt budget. (Mid-stream errors used to fall
                // through as an empty `finish_reason:"error"` delta and then
                // re-call the same provider unboundedly, one `step` per
                // failure, until `max_steps` — see `process_stream`.)
                let outcome: TurnStreamResult = match self
                    .router
                    .chat_completion(
                        &provider_id,
                        outgoing,
                        // `None`, not `Some(vec![])`: `openai_compat` writes the
                        // vec through unconditionally, and a bare `"tools": []`
                        // is a 400 on several OpenAI-compatible endpoints.
                        if in_wrapup {
                            None
                        } else {
                            Some(tools_for_turn.clone())
                        },
                        sampling,
                        Some(provider_model.clone()),
                        id_slot,
                        // The user is waiting on this one, and it is charged to
                        // the app that owns the session.
                        turn_app_id
                            .as_deref()
                            .unwrap_or(crate::provider::queue::DAEMON_LANE),
                        self.priority,
                        // Unconstrained, deliberately: a schema is applied to
                        // the final answer only, by `finalize_structured`
                        // after this loop. Constraining the tool loop itself
                        // would forbid tool calls outright on Anthropic (whose
                        // mechanism *is* a forced tool) — see
                        // `provider::schema`'s "Interaction with tools".
                        &ResponseSpec::text(),
                    )
                    .await
                {
                    Ok(s) => self.process_stream(s, event_tx).await,
                    Err(e) => Err(e),
                };
                match outcome {
                    Ok(result) => break result,
                    Err(e) => {
                        // Passive circuit breaker: a transport-class failure
                        // (connect error, header timeout, mid-stream drop —
                        // `Request`/`ConnectFailed`/`Timeout`, see
                        // `is_transport_error`) marks this provider unhealthy
                        // with a cooldown, so the failover re-resolution below
                        // — and any future unpinned turn — skips it until a
                        // health probe flips it back.
                        if e.is_transport_error() {
                            self.router.mark_unhealthy(&provider_id, &format!("{e}"));
                        }
                        // A fatal classification (bad key, exhausted billing,
                        // overlong context) won't be fixed by another attempt
                        // — fail fast instead of burning the budget and
                        // possibly triggering a pointless `ModelFailover`.
                        if attempt >= max_attempts || !e.is_retryable() {
                            // release-fixes item 27: `wire_type_tag` is `Some`
                            // for the classified cases the frontend can give
                            // real guidance on (context/credits/auth/network)
                            // — those go out as the distinct `provider_error`
                            // event so `error_type` actually reaches it,
                            // instead of the generic untagged `error` every
                            // chat_completion failure used to collapse into
                            // regardless of what was actually wrong.
                            // A context overflow that still got past the
                            // pre-flight guard means our idea of the window
                            // was wrong. The provider just told us the real
                            // one (`n_ctx`), so record it — otherwise the
                            // valve keeps budgeting against the same bad
                            // number and blows the window again next turn —
                            // and compact before returning, because this
                            // `return` is above the post-turn compaction pass
                            // and without it the session stays a wall.
                            if let ProviderError::ContextExceeded { context_window, .. } = &e {
                                if let Some(real) = context_window {
                                    if Some(*real) != self.router.context_length(&provider_id) {
                                        tracing::warn!(
                                            session_id,
                                            provider_id,
                                            real,
                                            "provider reports a different context window than \
                                             configured — correcting"
                                        );
                                        self.router.set_context_length(&provider_id, *real);
                                    }
                                }
                                let window = context_window.unwrap_or(context_length).max(1);
                                self.spawn_compaction(
                                    pool,
                                    session_id,
                                    &provider_id,
                                    Some(provider_model.clone()),
                                    window,
                                    true,
                                );
                            }
                            let tag = e.wire_type_tag();
                            let _ = event_tx.send(SSEEvent {
                                event_type: if tag.is_some() {
                                    SSEEventType::ProviderError
                                } else {
                                    SSEEventType::Error
                                },
                                error_message: Some(format!("{}", e)),
                                error_type: tag.map(String::from),
                                session_id: Some(session_id.to_string()),
                                is_last: true,
                                ..Default::default()
                            });
                            return;
                        }
                        // Jittered exponential backoff; a provider-supplied
                        // `Retry-After` hint (429/503) is honored as a floor
                        // so we don't hammer a rate-limited endpoint on our
                        // own (shorter) schedule.
                        let backoff = backoff_ms(self.fallback_cfg.retry_delay_ms, attempt);
                        let delay_ms = match e.retry_after() {
                            Some(secs) => backoff.max(secs.saturating_mul(1000)),
                            None => backoff,
                        };
                        if delay_ms > 0 {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        }
                        // Re-resolve — the router prefers a healthy provider,
                        // so if a background health check has since marked
                        // the one that just failed unhealthy (or another
                        // provider outranks it), this can pick a different
                        // one; otherwise it retries the same provider.
                        if let Ok(next_id) = self.resolve_provider(session_id, None).await {
                            // A failover that lands on a denied model is not a
                            // failover, it is the bypass. Staying put and
                            // retrying the provider that just failed is the
                            // lesser harm: it may recover, and if it does not
                            // the run ends with an error the caller can read
                            // rather than a bill it cannot.
                            let next_model = self.model_for(
                                &next_id,
                                effective_provider.as_deref(),
                                model_override,
                            );
                            if next_id != provider_id
                                && crate::agent::subagent_pick::is_denied(&next_model, &model_deny)
                            {
                                tracing::info!(
                                    session_id,
                                    from = %provider_id,
                                    to = %next_id,
                                    "declining a failover onto a denylisted subagent model"
                                );
                            } else if next_id != provider_id {
                                let _ = event_tx.send(SSEEvent {
                                    event_type: SSEEventType::ModelFailover,
                                    content: Some(format!(
                                        "Switching from provider '{provider_id}' to '{next_id}' \
                                         after error: {e}. The model changes with it — a model \
                                         pinned to '{provider_id}' does not apply to '{next_id}'."
                                    )),
                                    session_id: Some(session_id.to_string()),
                                    ..Default::default()
                                });
                                provider_id = next_id;
                                // The pin does not follow the failover either —
                                // same rule, and this site is the one that
                                // switches providers *mid-turn*, so a carried
                                // pin here changes the model under a
                                // conversation already in progress.
                                provider_model = self.model_for(
                                    &provider_id,
                                    effective_provider.as_deref(),
                                    model_override,
                                );
                            }
                        }
                    }
                }
            };

            let (content_buf, mut turn_tool_calls, finish_reason, turn_usage, timing) = turn_result;

            // Reasoning accounting. This — not the wire fields — is what makes
            // the cap real: three of the five dialects have no budget field at
            // all, including the self-hosted ones where an over-thinking
            // quantized model is most likely.
            if let Some(cap) = reasoning_cap {
                reasoning_spent = reasoning_spent.saturating_add(timing.reasoning_tokens);
                let budget = tokens::reasoning_budget_tokens(
                    cap,
                    Some(context_length),
                    projected_input,
                    report_reserve,
                    i32::MAX,
                );
                // `budget > 0` is not redundant with the comparison. A budget
                // below `MIN_REASONING_BUDGET` resolves to zero — which happens
                // at step 0 on any model whose window leaves less than a
                // thousand tokens of headroom once the report reserve is taken —
                // and `0 >= 0` then latched on a run that had not thought at
                // all, announcing "spent 0 of 0 tokens" and turning thinking off
                // before the first step. A budget of zero means there was never
                // room to think here, which the wrap-up valve already handles;
                // it is not a run that overspent.
                if !reasoning_exhausted && budget > 0 && reasoning_spent >= budget {
                    reasoning_exhausted = true;
                    tracing::info!(
                        session_id,
                        spent = reasoning_spent,
                        budget,
                        "reasoning budget exhausted; thinking disabled for the rest of this run"
                    );
                    // A latch, not an abort. The delegate still owes its caller
                    // a report, and one written without further thinking is far
                    // more useful than a turn that ended with nothing.
                    let notice = format!(
                        "Reasoning budget spent ({reasoning_spent} of {budget} tokens). \
                         Thinking is now off. Finish with what you have, and record anything \
                         you could not determine in `refusals`."
                    );
                    let _ = event_tx.send(SSEEvent {
                        event_type: SSEEventType::ToolFinish,
                        tool_name: Some(REASONING_BUDGET_TOOL.to_string()),
                        tool_result: Some(notice.clone()),
                        session_id: Some(session_id.to_string()),
                        ..Default::default()
                    });
                    messages.push(json!({"role": "system", "content": notice}));
                }
            }

            // A failover inside the attempt loop above is a real, announced
            // switch — carry it to the remaining steps instead of letting the
            // next iteration silently revert to the pinned provider.
            turn_provider_id = provider_id.clone();
            turn_provider_model = provider_model.clone();
            last_provider_id = Some(provider_id.clone());
            last_provider_model = Some(provider_model.clone());

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
                // Mark taken here, before the assistant message is pushed
                // below, so it cleanly separates "what the provider counted"
                // from "what we appended after" — that split is what makes the
                // next iteration's reserve check a delta rather than a full
                // re-encode of the transcript.
                last_usage = Some((
                    messages.len(),
                    i32::try_from(input_tokens).unwrap_or(i32::MAX),
                ));
                let output_tokens = usage_val
                    .get("output_tokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);

                let _ = self
                    .stats
                    .record_usage(
                        session_id,
                        // Saturating casts, not `as i32`: a provider reporting a
                        // token count over i32::MAX would otherwise wrap negative
                        // and record garbage in the cost estimate.
                        i32::try_from(input_tokens).unwrap_or(i32::MAX),
                        i32::try_from(output_tokens).unwrap_or(i32::MAX),
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
                tokens_per_second: timing.tokens_per_second,
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
                    messages.push(build_assistant_message(&content_buf, &turn_tool_calls));
                    // Those persisted `tool_calls` are never executed here, so
                    // without results the next provider request carried
                    // dangling tool_calls (HTTP 400 on OpenAI-compatible
                    // endpoints) — append a synthetic error result per call so
                    // the pairing stays valid (`build_aborted_tool_results`).
                    messages.extend(build_aborted_tool_results(
                        &turn_tool_calls,
                        ABORTED_FOR_STEP_BUDGET,
                    ));
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
                    // Re-clamp to the ceiling on EVERY grant: the ceiling is
                    // only applied to the initial value above, so an
                    // unbounded chain of `request_more_steps` calls would
                    // otherwise push `max_steps` past it, defeating the
                    // spend guard entirely.
                    max_steps = (max_steps + BUDGET_EXTENSION_STEPS as i64).min(MAX_STEPS_CEILING);
                }

                if turn_tool_calls.is_empty() {
                    // Same fix as above: this path used to `break`/`continue`
                    // without ever appending the assistant message.
                    messages.push(build_assistant_message(&content_buf, &turn_tool_calls));
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

            // Wrap-up completion. `in_wrapup` and `in_budget_check` are
            // mutually exclusive by `decide_turn_mode`, so the two
            // `messages.pop()` calls can never both run in one iteration —
            // that invariant is what makes "exactly one system message is
            // injected per iteration and it is always popped" checkable.
            if in_wrapup {
                // Popped for the same reason as the budget branch's pop. Note
                // this is NOT about persistence: `save_messages` skips
                // `role == "system"` entirely. It is about not leaving a stale
                // message to drift every later delta count, and about keeping
                // that invariant one line long.
                messages.pop();
                messages.push(wrapup_persist_shape(&content_buf, &turn_tool_calls));
                if let Err(e) = self.context.save_messages(session_id, &mut messages).await {
                    tracing::warn!("failed to save messages for session {session_id}: {e}");
                }
                // Unconditional — `finish_reason` is deliberately ignored.
                //
                // A clamped `max_tokens` makes `finish_reason: "length"` the
                // *likely* outcome for a chatty model, and the normal path
                // below only breaks on `stop`/`end_turn`. Falling through would
                // `step += 1; continue` with `projected_input` now larger,
                // whereupon the latch stops the re-injection but the full tool
                // set is offered again with less room than when we intervened —
                // the feature would cost a round trip and achieve nothing, then
                // burn to `max_steps`.
                //
                // Ignoring the finish reason is sound because another iteration
                // cannot acquire room, only consume it. Breaking here falls
                // through to the post-turn compaction spawn below, which
                // reclaims room for the user's *next* turn. The valve is a
                // graceful bridge to compaction, not a retry loop — which is
                // exactly what the system message promises the model.
                break;
            }

            // Add assistant message (reached for a non-budget-check turn, or
            // a budget-check turn that approved more steps and still has
            // real tool calls left to execute this same turn).
            messages.push(build_assistant_message(&content_buf, &turn_tool_calls));

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
                .execute_tools(
                    session_id,
                    &turn_tool_calls,
                    allowed_dirs,
                    session_cwd,
                    event_tx,
                )
                .await;

            for (tc, result) in turn_tool_calls.iter().zip(tool_results) {
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
        }

        // Cloned before the compaction block below moves `last_provider_id`/
        // `last_provider_model` — the title-derivation pass further down
        // needs its own copies of whichever provider/model actually handled
        // this turn.
        let title_provider_id = last_provider_id.clone();
        let title_provider_model = last_provider_model.clone();

        // Schema-constrained final answer.
        //
        // Runs after the tool loop rather than inside it: the loop must stay
        // unconstrained so the model can call tools, and every dialect's
        // structured-output mechanism is mutually exclusive with ordinary tool
        // use to some degree. So the answer is re-asked for, once, with tools
        // withheld and the schema attached — one extra cheap call in exchange
        // for a guarantee instead of a scrape.
        //
        // Deliberately also runs after a step-limit exit: a specialist that ran
        // out of budget still owes its caller a report of what it did get, and
        // a truncated structured answer is far more useful to the parent than
        // prose it cannot parse. It does not run for a vanished client, where
        // there is nobody to answer and the next request would be wasted work.
        if response_spec.is_constrained() && !event_tx.is_closed() {
            if let Some(pid) = title_provider_id.clone() {
                let provider_sampling = self.router.sampling(&pid);
                let sampling = match preset.as_ref() {
                    Some(p) => crate::provider::sampling::merge(p, &provider_sampling),
                    None => provider_sampling,
                };
                let model = title_provider_model
                    .clone()
                    .unwrap_or_else(|| self.router.resolve_model(&pid, model_override));
                self.finalize_structured(
                    session_id,
                    &mut messages,
                    &pid,
                    &model,
                    sampling,
                    turn_app_id
                        .as_deref()
                        .unwrap_or(crate::provider::queue::DAEMON_LANE),
                    &response_spec,
                    report_reserve,
                    &cache_directive,
                    event_tx,
                )
                .await;
            }
        }

        // Post-turn compaction check — ONCE per turn, fire-and-forget. This
        // used to be awaited inline on EVERY tool-loop iteration: O(steps ×
        // history) DB reads plus a full summarizer stall between tool steps,
        // against its own doc contract ("compaction fires fire-and-forget
        // after every turn"). The CAS compaction lock inside
        // `run_compaction` is the overlap guard if passes for the same
        // session ever race. The provider's own `context_length` (Settings →
        // Providers → Advanced) wins when set; fall back to the daemon-wide
        // `token_management.max_context_tokens` otherwise.
        if let Some(pid) = last_provider_id {
            let context_length = self
                .router
                .context_length(&pid)
                .unwrap_or(self.context.config().max_context_tokens);
            self.spawn_compaction(
                pool,
                session_id,
                &pid,
                last_provider_model,
                context_length,
                false,
            );
        }

        // Post-turn title derivation (release-fixes item 12) — ONCE, only
        // for a session that isn't already named (re-checked here, not just
        // at send time: this deliberately fires after the turn completes,
        // not when the user sends their first message, so the summarizer
        // has an actual exchange to work from rather than just the raw
        // first message). Fire-and-forget like the two passes above — a
        // slow or failed title call must never hold up the turn's own
        // completion event.
        {
            let pool = pool.clone();
            let session_id = session_id.to_string();
            // Kept back from the move below so the spawned task can still be
            // registered against the session that owns it.
            let tracked_session = session_id.clone();
            let summarizer = self.summarizer.clone();
            let event_tx = event_tx.clone();
            let handle = tokio::spawn(async move {
                derive_and_set_title(
                    &pool,
                    &session_id,
                    &summarizer,
                    title_provider_id.as_deref(),
                    title_provider_model,
                    &event_tx,
                )
                .await;
            });
            self.track_background(&tracked_session, handle.abort_handle());
        }

        // Turn-end Adaptive Pathway pass (runs once per turn, fire-and-forget):
        // every `learn_every_n` exchanges, run the LLM extraction learn pass
        // over the session's unlearned tail. `extract_and_record` re-checks
        // the per-session learn lock, pause state, and its forward-only
        // watermark, so the redundant outer guards stay light. Doesn't block
        // the turn or perturb prompt caching.
        //
        // This used to also synthesize a belief directly from each
        // successful tool call ("User got positive result from {tool}:
        // {context}") -- removed. That's a fact about a *tool*, not the
        // user, which is exactly what the extraction prompt explicitly
        // instructs the LLM extractor never to record; hard-coding the same
        // violation on this separate path undermined it regardless of what
        // the prompt said, and surfaced tool-usage trivia in `[What I know
        // about you]` as if it were a personality trait.
        // Resolved per turn from the session's owning app, not held as one
        // daemon-wide handle: learning writes into that app's graph and no
        // other's.
        let engine = match self.app_scope(session_id).await.0 {
            Some(app_id) => self.plugins.pathway_for(&app_id).await,
            None => None,
        };
        // `learn_every_n` is a user-editable u32 (`PathwayConfig::learn_every_n`).
        // A 0 (or negative-after-cast) value would make `%` below divide by
        // zero — a guaranteed panic inside this spawned task on the very
        // first turn — and what the cadence was *meant* to mean ("never
        // learn") is at any rate not "learn on every turn". Clamp to the
        // default cadence.
        let learn_every_n = self.pathway_cfg.learn_every_n.max(1);
        let host_pool = pool.clone();
        let chat = self.summarizer.clone();
        let learn_session_id = session_id.to_string();
        // Kept back from the move below so the spawned task can still be
        // registered against the session that owns it.
        let learn_session_id_for_tracking = learn_session_id.clone();
        let handle = tokio::spawn(async move {
            let Some(engine) = engine else { return };
            if engine.is_paused(&learn_session_id).await.unwrap_or(false) {
                return;
            }
            // Bump `pathway.db`'s own per-session exchange counter --
            // `PathwayEngine::recall`'s `[Where I'm unsure]` cadence gate
            // reads this same `conversation_state.exchange_count` field
            // (`unsure_due`, every 12 exchanges), and nothing anywhere else
            // in the daemon incremented it: that section could never
            // actually fire. Reusing the returned count for the learn
            // cadence below also replaces what used to be a separate
            // `COUNT(*) FROM messages` query issued every single turn
            // (against the host db, not even the pathway one) purely to
            // recompute a number `pathway.db` already tracks incrementally.
            // On a DB error, skip this turn's learn pass entirely rather than
            // falling back to 0 — `0 % N == 0` would otherwise make the
            // cadence gate "learn on every turn", and the bump's failure is
            // in no way a signal to alter cadence.
            let Ok(exchange_count) = engine.db.bump_exchange(&learn_session_id).await else {
                return;
            };
            // The MAX(rowid) guard below is redundant with
            // `extract_and_record`'s watermark, so we skip re-deriving it here.
            if exchange_count % learn_every_n as i64 == 0 {
                let max_rowid: i64 =
                    sqlx::query_scalar("SELECT MAX(rowid) FROM messages WHERE session_id = ?")
                        .bind(&learn_session_id)
                        .fetch_one(&host_pool)
                        .await
                        .unwrap_or(0);
                if max_rowid > 0 {
                    let _ = adaptive_pathway::learn::extract_and_record(
                        &engine,
                        &host_pool,
                        chat.as_ref(),
                        adaptive_pathway::learn::LearnRequest {
                            session_id: &learn_session_id,
                            through_rowid: max_rowid,
                            given_chunk: None,
                        },
                        adaptive_pathway::learn::LearnTrigger::TurnEnd,
                    )
                    .await;
                }
            }
        });
        self.track_background(&learn_session_id_for_tracking, handle.abort_handle());
    }

    /// Adaptive Pathway turn-start hook: in-process recall against the
    /// `PathwayEngine`. Replaces the old MCP-based `decide` call. Delegates
    /// to `PathwayEngine::recall`, which selects ≤6 beliefs via DPP grounded
    /// in the current user query (capped at `MAX_CANDIDATES` so cost stays
    /// bounded as the store grows), filters suppressed beliefs, routes by
    /// inferred domain, and renders the full `[Working assumptions about you]` +
    /// `[Worth testing this turn]` + `[Where I'm unsure]` + `[Check
    /// yourself]` block within the token budget. Wrapped in the same
    /// timeout budget the embed step alone used to get — now bounding the
    /// whole call (embed + several small DB reads), since a cold/down
    /// Ollama or a slow query must degrade to `None` (zero prompt delta)
    /// rather than stall the turn. Returns `None` when the engine is
    /// absent, paused, has nothing to say, or the budget is exceeded.
    /// Decide the pathway-recall path for this turn and produce its
    /// rendered text as `(ap_hints, thought_seed)` — **at most one is ever
    /// `Some`**. Picks `recall_thought_seed` (a trailing assistant `<think>`
    /// prefill — see `context::builder::build_messages`'s `thought_seed`
    /// param) only when both hold: the resolved provider has confirmed it
    /// honors a trailing assistant-role prefill
    /// (`Provider::supports_assistant_prefill` — protocol-native for
    /// Anthropic, an explicit user opt-in for everything else, never
    /// assumed), and the resolved model's name matches the reasoning-model
    /// heuristic (`reasoning_models::supports_reasoning` — seeding a
    /// `<think>` block into a model with no real thinking phase would leak
    /// the seed's raw framing into the visible answer). Otherwise falls back
    /// to today's `recall` → `ap_hints` system-block path. Both `None` on a
    /// disabled engine, timeout, or no match — zero prompt delta either way.
    async fn pathway_recall(
        &self,
        session_id: &str,
        user_message: &str,
        provider_id: &str,
        model: &str,
    ) -> (Option<String>, Option<String>) {
        let engine_owned = match self.app_scope(session_id).await.0 {
            Some(app_id) => self.plugins.pathway_for(&app_id).await,
            None => None,
        };
        let Some(engine) = engine_owned.as_ref() else {
            return (None, None);
        };
        let seed_eligible = self.router.supports_assistant_prefill(provider_id)
            && reasoning_models::supports_reasoning(model);
        if seed_eligible {
            let seed = tokio::time::timeout(
                Duration::from_millis(AP_RECALL_EMBED_BUDGET_MS),
                engine.recall_thought_seed(session_id, user_message),
            )
            .await
            .ok()
            .flatten();
            (None, seed)
        } else {
            let hints = tokio::time::timeout(
                Duration::from_millis(AP_RECALL_EMBED_BUDGET_MS),
                engine.recall(session_id, user_message),
            )
            .await
            .ok()
            .flatten();
            (hints, None)
        }
    }

    /// Pre-flight memory recall hook, mirroring `pathway_recall`'s
    /// cache-aware tail-injection shape. Runs `agent::memory::preflight_recall`
    /// and records the daemon-wide counters (`total`/`injected`) for the
    /// settings readout. Any failure, disabled recall, or miss returns `None`
    /// (zero prompt delta).
    async fn preflight_recall(
        &self,
        session_id: &str,
        user_message: &str,
        compacted_through: i64,
    ) -> Option<String> {
        let pool = self.context.pool().clone();
        let injected = match preflight_recall(
            &pool,
            session_id,
            user_message,
            compacted_through,
            &self.memory_cfg,
        )
        .await
        {
            Ok(Some(block)) => Some(block),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("memory preflight failed for {session_id}: {e}");
                None
            }
        };
        self.preflight.record(
            self.memory_cfg.preflight_enabled && compacted_through > 0,
            injected.is_some(),
        );
        injected
    }

    async fn process_stream(
        &self,
        mut stream: Pin<Box<dyn Stream<Item = Delta> + Send>>,
        event_tx: &mpsc::UnboundedSender<SSEEvent>,
    ) -> Result<
        (
            String,
            Vec<ToolCall>,
            Option<String>,
            Option<Value>,
            TimingResult,
        ),
        ProviderError,
    > {
        // One growing buffer rather than one heap `String` per streamed
        // delta: the vec was only ever `join("")`d, so the per-token
        // allocations bought nothing and were held for the whole call.
        let mut content_buf = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut finish_reason: Option<String> = None;
        let mut usage: Option<Value> = None;
        let mut timing = TimingResult::default();

        let start = Instant::now();
        let mut first_token = true;
        let mut token_count = 0;
        let mut content_chars = 0usize;
        // Buffered and tokenized once at end of stream rather than per delta:
        // `count_text_tokens` runs a real BPE encode, and doing that on every
        // fragment of a long think would cost more than the think.
        let mut reasoning_buf = String::new();

        while let Some(mut delta) = stream.next().await {
            // A mid-stream provider failure (connection drop, idle timeout,
            // or an SSE `error` event) arrives as a Delta with
            // `error_type == "request"` (see `openai_compat.rs`/`anthropic.rs`
            // transient-error emission). It used to fall through as an empty
            // `finish_reason: "error"` delta, and the turn then treated a
            // non-`stop` finish as "another step" — re-calling the same
            // provider with no backoff, no failover, and no shared retry
            // budget, up to `max_steps` times per turn. Surface it as a
            // `ProviderError` instead so the caller's retry/failover block
            // handles it exactly like any other transient failure.
            if delta.error_type.as_deref() == Some("request") {
                return Err(ProviderError::Request {
                    user_message:
                        "Provider stream failed mid-response (connection dropped or idle timeout)"
                            .to_string(),
                    raw_message: format!(
                        "finish_reason={:?} error_type={:?}",
                        delta.finish_reason, delta.error_type
                    ),
                    http_status: 0,
                });
            }

            if first_token {
                timing.ttfb_ms = start.elapsed().as_secs_f64() * 1000.0;
                timing.ttft_ms = start.elapsed().as_secs_f64() * 1000.0;
                first_token = false;
            }

            if let Some(content) = delta.content.take() {
                if !content.is_empty() {
                    // Count characters, not bytes: the backstop threshold
                    // (`MAX_TURN_CONTENT_CHARS`) is documented in characters,
                    // and `content.len()` (byte count) inflated multi-byte
                    // UTF-8 ~3x, so a legitimate long CJK/emoji reply could
                    // be cut off early.
                    content_chars += content.chars().count();
                    content_buf.push_str(&content);
                    token_count += 1;
                    // One clone per delta, not two: the accumulator now grows a
                    // single buffer (it was a `Vec<String>` that only ever got
                    // `join("")`d, i.e. one small heap allocation per token held
                    // for the whole call), so `content` itself can move into the
                    // event.
                    let _ = event_tx.send(SSEEvent {
                        event_type: SSEEventType::LlmDelta,
                        content: Some(content),
                        ..Default::default()
                    });
                }
            }

            if let Some(reasoning) = delta.reasoning.take() {
                if !reasoning.is_empty() {
                    // Reasoning counts toward the same ceiling as content —
                    // a thinking-loop stream is exactly the unbounded-output
                    // case `MAX_TURN_CONTENT_CHARS` exists to catch, and
                    // hosted providers get no max_tokens floor.
                    content_chars += reasoning.chars().count();
                    reasoning_buf.push_str(&reasoning);
                    // ...and toward the same delta count. This is only the
                    // fallback for a provider that reports no usage at all,
                    // but leaving thinking out of it meant a reasoning model
                    // on such a provider had most of its generated output
                    // missing from the measured rate.
                    token_count += 1;
                    let _ = event_tx.send(SSEEvent {
                        event_type: SSEEventType::ReasoningDelta,
                        content: Some(reasoning),
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
        // is actually a count of non-empty SSE deltas, not tokens (a single
        // delta can be a sub-token fragment or bundle several tokens depending
        // on the provider's streaming granularity), and was misleadingly
        // reported as `total_tokens` in LlmTiming/the timings table. Only fall
        // back to the delta count when the provider genuinely didn't report
        // usage.
        timing.total_tokens = usage
            .as_ref()
            .and_then(output_tokens_including_reasoning)
            .unwrap_or(token_count);
        // The provider's own count when it reports one — it knows what it
        // charged for. Otherwise the tokenizer over what we actually received,
        // which under-counts for CJK and some code. Erring low is deliberate:
        // a budget that trips late costs some tokens, while one that trips
        // early cuts off a legitimately hard question.
        timing.reasoning_tokens = usage
            .as_ref()
            .and_then(|u| u.get("reasoning_tokens"))
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .unwrap_or_else(|| tokens::count_text_tokens(&reasoning_buf));
        timing.finalize_rate();

        Ok((content_buf, tool_calls, finish_reason, usage, timing))
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
        session_cwd: Option<&str>,
        event_tx: &mpsc::UnboundedSender<SSEEvent>,
    ) -> Vec<String> {
        let semaphore = Arc::new(Semaphore::new(self.max_concurrent_tool_calls.max(1)));

        let futures = tool_calls.iter().map(|tc| {
            // `filter(non-empty)` as well as `unwrap_or`: the key is always
            // present in a streamed call, so an empty name read as `""` rather
            // than falling through to "unknown" — which made the failure
            // invisible to every log filter looking for the latter.
            let tool_name = tc
                .function
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .unwrap_or("unknown")
                .to_string();
            let mut tool_args = tc.function.get("arguments").cloned().unwrap_or(json!({}));
            // Before containment and HITL, so both judge the path the tool
            // will really open rather than a bare "." the tool process would
            // resolve against its own (wrong) working directory.
            if let Some(cwd) = session_cwd {
                if crate::agent::sandbox::qualify_relative_path_args(
                    &tool_name,
                    &mut tool_args,
                    cwd,
                ) {
                    tracing::debug!(
                        session_id,
                        tool_name,
                        cwd,
                        "qualified a relative tool path against the session working directory"
                    );
                }
            }
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
    ///
    /// One deliberate exception: a *write-class* tool that resolves to a path
    /// outside the session's allowed directories is hard-denied instead of
    /// escalated (`is_write_tool`). The user is the security boundary, but
    /// writes escaping the chat/`cache_dir`/temp working set are a policy
    /// violation, not an ask-the-human question — modeling that with a prompt
    /// would just train the model to attempt out-of-scope writes more often.
    /// Read-class tools keep the old behavior (force-approval).
    #[allow(clippy::too_many_arguments)]
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

        // The provider stream substitutes `{"__error": "..."}` for arguments
        // it couldn't parse (truncated/malformed streamed JSON) rather than
        // silently defaulting to `{}` — see `openai_compat.rs`/`anthropic.rs`
        // `flush_tool_call_buf`. That sentinel must short-circuit here,
        // BEFORE HITL/containment/execution: passing it through to the real
        // tool runs it with a garbage args object (e.g. `read_file(path:
        // undefined)`), and whether that "coincidentally" fails depends on
        // the tool's own schema validation, not on anything this loop
        // enforces. Fail the call outright and surface why.
        if let Some(msg) = tool_args.get("__error").and_then(|v| v.as_str()) {
            let err = format!("Tool {tool_name} call failed: {msg}");
            let _ = event_tx.send(SSEEvent {
                event_type: SSEEventType::ToolFinish,
                tool_name: Some(tool_name),
                tool_result: Some(err.clone()),
                session_id: Some(session_id.to_string()),
                ..Default::default()
            });
            return err;
        }

        // Enforce the per-run allow-list. The advertised tool set was already
        // narrowed to it in `run_inner`, but a model can call a tool it was
        // never offered — from a stale prompt prefix, a cached tool block, or
        // plain invention — so the restriction is re-checked here, where it is
        // load-bearing. A specialist's tool list is a boundary, not a hint.
        //
        // Denied before containment and HITL for the same reason the write
        // hard-deny below runs first: this is a policy violation, not a
        // question to put to a human, and reaching the approval path would let
        // an approval grant what the definition forbids.
        if let Some(allow) = self.tool_allow.as_ref() {
            if !allow.contains(&tool_name) && tool_name != BUDGET_TOOL {
                let err = format!(
                    "Tool {tool_name} is not available to this run. Available: {}.",
                    {
                        let mut names: Vec<&str> = allow.iter().map(|s| s.as_str()).collect();
                        names.sort_unstable();
                        names.join(", ")
                    }
                );
                let _ = event_tx.send(SSEEvent {
                    event_type: SSEEventType::ToolFinish,
                    tool_name: Some(tool_name),
                    tool_result: Some(err.clone()),
                    session_id: Some(session_id.to_string()),
                    ..Default::default()
                });
                return err;
            }
        }

        // Hard-deny a *write-class* tool that resolves to a path outside the
        // session's allowed directories — checked unconditionally, BEFORE the
        // HITL decision below. It only needs tool_name/args/allowed_dirs,
        // and running it first means an out-of-scope write can neither slip
        // through when HITL would decide `needs_approval` (the common
        // `always_ask` case previously reached the approval path, letting a
        // user approve a write that escapes chat_dir/current dir despite the
        // module docs declaring such writes "hard-denied ... not
        // escalated"), nor leave a phantom pending action behind:
        // `check_tool_call_with_rules`'s `always_ask` side effect REGISTERS
        // a pending action, which a post-hoc deny then orphaned in the
        // pending-approvals API for ~1h with no HitlPause ever emitted. This
        // is a policy violation, not an ask-the-human question.
        if is_write_tool(&tool_name)
            && !check_containment(&tool_args, allowed_dirs, self.sandbox_strict)
        {
            // Name the directories. The old message said only that the path
            // was "outside this session's allowed directories" without saying
            // where the model *could* write, which left it guessing — and a
            // guessing model retries, so an unhelpful denial costs a round
            // trip per attempt. `allowed_dirs` also carries scratch/cache
            // entries that are true but useless to suggest, so list the first
            // few (chat_dir and cwd lead, see `allowed_dirs_for_session`).
            let suggestions = allowed_dirs
                .iter()
                .take(2)
                .cloned()
                .collect::<Vec<_>>()
                .join(" or ");
            let err = if suggestions.is_empty() {
                format!(
                    "Tool {tool_name} denied: it would write to a path outside this \
                     session's allowed directories"
                )
            } else {
                format!(
                    "Tool {tool_name} denied: it would write outside this session's \
                     allowed directories. Write to {suggestions} instead."
                )
            };
            let _ = event_tx.send(SSEEvent {
                event_type: SSEEventType::ToolFinish,
                tool_name: Some(tool_name),
                tool_result: Some(err.clone()),
                session_id: Some(session_id.to_string()),
                ..Default::default()
            });
            return err;
        }

        // Which app owns this session, resolved once for the rest of the call:
        // it scopes both the HITL rules consulted below and the tool registry
        // dispatched into. A primary-key lookup against a WAL database, next
        // to a tool call that will take orders of magnitude longer.
        //
        // An unresolvable owner falls through to `""`, which matches no app's
        // private rules and no app's private servers -- so such a call gets
        // the shared pool and full approval prompts, rather than either
        // failing outright or inheriting someone else's consent.
        let caller_app = crate::storage::sessions::owner_of(&self.pool, session_id)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();

        // Resolve the HITL decision without holding the shared mutex across
        // `check_tool_call`'s DB rule query: `check_tool_call_with_rules` is
        // synchronous, but the rule lookup itself is an `.await` on the pool —
        // holding the lock across it previously serialized every concurrent
        // tool call in a session (and could stall the loop behind a slow DB).
        // The pool handle is cloned out under a brief lock (clone, not await),
        // the query runs lock-free, then the decision is applied under a
        // short-lived lock.
        let mut decision = {
            let rules = {
                let hitl = self.hitl.lock().await;
                hitl.pool().clone()
            };
            let rules = hitl_rules::list_rules_by_tool(&rules, &caller_app, &tool_name)
                .await
                .unwrap_or_default();
            let mut hitl = self.hitl.lock().await;
            hitl.check_tool_call_with_rules(session_id, &tool_name, &tool_args, &rules)
        };

        let mut escalated_by_containment = false;
        if (decision.action == "proceed" || decision.action == "always_allow")
            && !check_containment(&tool_args, allowed_dirs, self.sandbox_strict)
        {
            escalated_by_containment = true;
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

        // An unattended run whose definition already named this tool, on a call
        // nobody has otherwise decided anything about, proceeds.
        //
        // Without this every specialist that uses a tool fails, always. The
        // shipped default policy is `always_ask` and a delegate has no
        // approver, so `needs_approval` became a refusal on the first call of
        // every run -- the researcher could not search, the summarizer could
        // not read. Worse than failing: a model handed "the tool is blocked"
        // mid-task does not reliably stop, and one observed run answered a
        // literature-search request out of its own memory, fabricating the
        // citations it had been unable to look up.
        //
        // `tool_allow` is not a hint here -- it is a decision the user made
        // when they wrote the specialist, in advance and in writing, which is
        // exactly what the approval prompt would have asked them for. It has
        // already been enforced twice by this point (the advertised tool set
        // was narrowed to it in `run_inner`, and the dispatch check above
        // re-tested it), so reaching here means the call is on the list. The
        // delegate also inherits the parent's grants and nothing more, so it
        // can reach nothing its parent could not.
        //
        // Deliberately narrow. `from_default_policy` is true only when no
        // pattern and no stored rule matched, so a user's explicit `reject`
        // rule still rejects, a rule this code cannot parse still fails closed,
        // and a path outside the session's allowed directories still refuses
        // (`escalated_by_containment`). Those are decisions; this covers only
        // the absence of one.
        let preauthorized_for_unattended_run = decision.action == "needs_approval"
            && self.hitl_auto_reject
            && decision.from_default_policy
            && !escalated_by_containment
            && self
                .tool_allow
                .as_ref()
                .is_some_and(|allow| allow.contains(&tool_name));
        if preauthorized_for_unattended_run {
            // Drop the pending record the decision registered as a side effect;
            // nothing will ever approve it, and leaving it behind advertises an
            // approval that no waiter will honor until `sweep_stale` reaps it.
            if let Some(action_id) = decision.pending_action_id.as_deref() {
                let mut hitl = self.hitl.lock().await;
                hitl.remove_pending(action_id);
            }
            tracing::debug!(
                session_id,
                tool_name,
                "unattended run: proceeding on a tool its definition allows"
            );
        }

        if decision.action == "needs_approval" && !preauthorized_for_unattended_run {
            let action_id = decision.pending_action_id.clone().unwrap_or_default();

            // No approver exists for this run, so there is nothing to wait for.
            // Refuse now and say why: the refusal reaches the model as this
            // call's result, which is what lets a specialist report what it was
            // blocked from doing rather than quietly returning a thinner answer
            // than its caller thinks it got.
            //
            // The pending record is dropped as well. `check_tool_call_with_rules`
            // registers one as a side effect of deciding `needs_approval`, and
            // leaving it behind would advertise an approval no waiter will ever
            // honor in the pending-approvals API until `sweep_stale` reaps it.
            if self.hitl_auto_reject {
                {
                    let mut hitl = self.hitl.lock().await;
                    hitl.remove_pending(&action_id);
                }
                let err = format!(
                    "Tool {tool_name} requires human approval, which is unavailable in an                      unattended run. It was not executed; report this in your answer."
                );
                let _ = event_tx.send(SSEEvent {
                    event_type: SSEEventType::ToolFinish,
                    tool_name: Some(tool_name.clone()),
                    tool_result: Some(err.clone()),
                    session_id: Some(session_id.to_string()),
                    ..Default::default()
                });
                return err;
            }

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
                // Remove the pending record AND any decision that raced in
                // after the timeout: the pending-approvals API would
                // otherwise show an approval no waiter will ever honor for
                // ~1h (until `sweep_stale` reaps it), and a late decision
                // must not resolve a call that has already failed closed.
                let mut hitl = self.hitl.lock().await;
                hitl.remove_pending(&action_id);
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

        // The pathway MCP server's `record`/`forget` need to know which
        // session they're acting on, but an in-process MCP connection is
        // daemon-lifetime while sessions rotate and stream concurrently --
        // so the server cannot hold a single "current session" without
        // racing. Inject it here instead, where the executing session is
        // unambiguous. `session_id` is hidden from the tool's advertised
        // schema (`#[schemars(skip)]`), so the model never sees or supplies
        // it; this is the only writer.
        //
        // `specialists`' two tools need the same injection for the same reason,
        // and one more besides: the session they are told about becomes the
        // *parent* of whatever delegate they start. A model-supplied id there
        // would let a turn graft its subagents onto another session's tree, so
        // this is the only writer for those as well.
        let needs_session = crate::mcp::builtin::PATHWAY_TOOLS.contains(&tool_name.as_str())
            || crate::specialists::server::SESSION_SCOPED_TOOLS.contains(&tool_name.as_str());
        let tool_args = if needs_session {
            let mut args = tool_args.clone();
            if let Some(obj) = args.as_object_mut() {
                obj.insert("session_id".to_string(), json!(session_id));
            }
            args
        } else {
            tool_args
        };
        // Dispatch through the caller's own view of the registry, so a tool
        // name shadowed across apps resolves to *this* app's server -- and a
        // name only another app has resolves to nothing.
        let result = self
            .mcp
            .execute_tool_for_app(&caller_app, &tool_name, &tool_args, None)
            .await;

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
mod backoff_tests {
    use super::backoff_ms;

    #[test]
    fn backoff_doubles_the_cap_each_attempt() {
        // Partial jitter: sleep is in [cap/2, cap). Bounds must double per
        // attempt from the base (1000ms).
        for attempt in 1..=4u32 {
            let b = backoff_ms(1000, attempt);
            assert!(b >= 500, "attempt {attempt}: got {b}");
            assert!(b < 1000 << (attempt - 1), "attempt {attempt}: got {b}");
        }
    }

    #[test]
    fn backoff_caps_at_the_ceiling() {
        // Attempt 30 would be 2^29 * base — far past the 60s cap; the sleep
        // must stay under MAX_BACKOFF_MS.
        for attempt in [20u32, 30, 50] {
            let b = backoff_ms(1000, attempt);
            assert!(b < 60_000, "attempt {attempt}: got {b}");
        }
    }

    #[test]
    fn backoff_is_always_within_its_own_cap() {
        for delay in [1u64, 250, 1000, 10_000] {
            for attempt in 1..=10u32 {
                let b = backoff_ms(delay, attempt);
                let cap = (delay.max(1) << (attempt - 1).min(16)).min(60_000);
                assert!(b <= cap, "delay {delay} attempt {attempt}: {b} > cap {cap}");
            }
        }
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
    use super::{sanitize_boolean_subschemas, tools_to_openai_format};
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
}

#[cfg(test)]
mod write_tool_tests {
    use super::{is_write_tool, WRITE_TOOL_NAMES};

    #[test]
    fn classifies_every_known_write_tool() {
        for name in WRITE_TOOL_NAMES {
            assert!(
                is_write_tool(name),
                "{name} should be classified as a write tool"
            );
        }
    }

    #[test]
    fn read_tools_are_not_write_tools() {
        for name in [
            "lean_file_read",
            "lean_excel_inspect",
            "lean_pdf_read_text",
            "lean_analyze_workspace",
            "lean_web_search",
            "decide",
        ] {
            assert!(!is_write_tool(name), "{name} should NOT be a write tool");
        }
    }
}

#[cfg(test)]
mod output_token_tests {
    use super::*;
    use serde_json::json;

    /// OpenAI's spec counts reasoning tokens *inside* `completion_tokens` and
    /// reports the breakdown for information only. Summing would double count.
    #[test]
    fn reasoning_already_inside_the_total_is_not_added_again() {
        let usage = json!({"output_tokens": 300, "reasoning_tokens": 200});
        assert_eq!(output_tokens_including_reasoning(&usage), Some(300));
    }

    /// Servers that report `completion_tokens` as the visible completion alone
    /// give themselves away: reasoning cannot exceed a total it is part of.
    /// This is the shape that made a reasoning model read a third of its real
    /// speed.
    #[test]
    fn reasoning_reported_outside_the_total_is_added() {
        let usage = json!({"output_tokens": 100, "reasoning_tokens": 200});
        assert_eq!(output_tokens_including_reasoning(&usage), Some(300));
        // Equal counts are also impossible for a subset of a non-zero total.
        let usage = json!({"output_tokens": 50, "reasoning_tokens": 50});
        assert_eq!(output_tokens_including_reasoning(&usage), Some(100));
    }

    #[test]
    fn a_non_reasoning_response_is_unaffected() {
        let usage = json!({"output_tokens": 42});
        assert_eq!(output_tokens_including_reasoning(&usage), Some(42));
        let usage = json!({"output_tokens": 42, "reasoning_tokens": 0});
        assert_eq!(output_tokens_including_reasoning(&usage), Some(42));
    }

    /// No reported output tokens at all means the caller must fall back to its
    /// own delta count, not to zero.
    #[test]
    fn missing_usage_reports_nothing_rather_than_zero() {
        assert_eq!(output_tokens_including_reasoning(&json!({})), None);
        assert_eq!(
            output_tokens_including_reasoning(&json!({"input_tokens": 10})),
            None
        );
    }

    /// An absurd reported value must clamp, not wrap negative.
    #[test]
    fn an_absurd_reported_count_saturates() {
        let usage = json!({"output_tokens": i64::MAX, "reasoning_tokens": i64::MAX});
        assert_eq!(output_tokens_including_reasoning(&usage), Some(i32::MAX));
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
mod budget_abort_tests {
    use super::{build_aborted_tool_results, ToolCall, ABORTED_FOR_STEP_BUDGET};
    use serde_json::json;

    fn pending_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            r#type: "function".into(),
            function: json!({"name": name, "arguments": {}}),
        }
    }

    /// The budget-abort branch persists an assistant message carrying
    /// tool_calls that are never executed — without synthetic `tool` results,
    /// the next provider request has dangling tool_calls (HTTP 400 on
    /// OpenAI-compatible endpoints). `build_aborted_tool_results` must emit
    /// exactly one error result per pending call, keyed by `tool_call_id`.
    #[test]
    fn one_error_result_per_pending_call_keyed_by_tool_call_id() {
        let calls = vec![
            pending_call("call_1", "read_file"),
            pending_call("call_2", "shell_run"),
        ];
        let results = build_aborted_tool_results(&calls, ABORTED_FOR_STEP_BUDGET);

        assert_eq!(results.len(), 2);
        for (call, result) in calls.iter().zip(&results) {
            assert_eq!(result["role"], "tool");
            assert_eq!(result["tool_call_id"], call.id);
            assert!(
                result["content"].as_str().unwrap().contains("cancelled"),
                "each call should carry an explanatory error result"
            );
        }
    }

    /// The reason is a parameter now — the wrap-up valve aborts for a
    /// different cause than the step budget, and telling the model the wrong
    /// one is worse than saying nothing.
    #[test]
    fn the_reason_is_carried_through_verbatim() {
        let calls = vec![pending_call("call_1", "read_file")];
        let results = build_aborted_tool_results(&calls, "[custom reason]");
        assert_eq!(results[0]["content"], "[custom reason]");
    }

    #[test]
    fn no_tool_calls_means_no_tool_results() {
        assert!(build_aborted_tool_results(&[], ABORTED_FOR_STEP_BUDGET).is_empty());
    }
}

#[cfg(test)]
mod wrapup_valve_tests {
    use super::{decide_turn_mode, wrapup_persist_shape, ToolCall, TurnMode};
    use serde_json::json;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            r#type: "function".into(),
            function: json!({"name": "read_file", "arguments": {}}),
        }
    }

    #[test]
    fn an_ordinary_step_gets_no_intervention() {
        assert_eq!(decide_turn_mode(0, false, false), TurnMode::Normal);
        assert_eq!(decide_turn_mode(7, false, false), TurnMode::Normal);
        // Step 0 never trips the step nudge, whatever the modulus says.
        assert_eq!(decide_turn_mode(0, true, false), TurnMode::Normal);
    }

    #[test]
    fn the_step_nudge_still_fires_on_multiples_of_twenty() {
        assert_eq!(decide_turn_mode(20, false, false), TurnMode::StepNudge);
        assert_eq!(decide_turn_mode(40, false, false), TurnMode::StepNudge);
    }

    /// The collision case, and the reason this is a function rather than an
    /// `if/else if`: offering `request_more_steps` on the same request that
    /// withdraws every tool is incoherent, and granting 20 more *steps* is a
    /// non-answer when the exhausted resource is *context*.
    #[test]
    fn context_exhaustion_outranks_the_step_nudge() {
        assert_eq!(decide_turn_mode(20, false, true), TurnMode::WrapUp);
    }

    /// No `step > 0` guard: a provider whose real window is smaller than the
    /// daemon-wide budget can be over the reserve before any tool has run, and
    /// suppressing the valve there would hand the provider the request that
    /// 400s instead.
    #[test]
    fn the_valve_can_fire_before_any_tool_has_run() {
        assert_eq!(decide_turn_mode(0, false, true), TurnMode::WrapUp);
    }

    /// Once issued, it never re-issues — the latch behind the unconditional
    /// `break`. (Dead in practice because the break ends the turn; it pins the
    /// latch's meaning against a later refactor that removes the break.)
    #[test]
    fn the_latch_prevents_a_second_wrap_up() {
        assert_eq!(decide_turn_mode(7, true, true), TurnMode::Normal);
        assert_eq!(decide_turn_mode(20, true, true), TurnMode::StepNudge);
    }

    #[test]
    fn prose_is_persisted_as_written() {
        let msg = wrapup_persist_shape("Here is the answer.", &[]);
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["content"], "Here is the answer.");
    }

    /// A model can emit a tool call despite being offered none. Persisting it
    /// would leave `tool_calls` with no `tool` role following, which is a hard
    /// 400 on the *next* turn's first request — and this branch breaks, so
    /// there is no later iteration that could ever supply the results.
    #[test]
    fn tool_calls_are_stripped_rather_than_persisted_dangling() {
        let msg = wrapup_persist_shape("Wrapping up.", &[call("c1"), call("c2")]);
        assert!(
            msg.get("tool_calls").is_none(),
            "a wrap-up reply must never persist dangling tool_calls: {msg}"
        );
        assert_eq!(msg["content"], "Wrapping up.");
    }

    /// The case stripping makes reachable: a reply that was *only* a tool call
    /// becomes `{"content": ""}` with no `tool_calls`, which several backends
    /// reject outright on the next request.
    #[test]
    fn an_empty_reply_still_carries_content() {
        for text in ["", "   "] {
            let msg = wrapup_persist_shape(text, &[call("c1")]);
            assert!(
                !msg["content"].as_str().unwrap().trim().is_empty(),
                "empty content is rejected by several backends: {msg}"
            );
            assert!(msg.get("tool_calls").is_none());
        }
        // ...and with no tool calls either, which is the "model said nothing
        // at all" case.
        let msg = wrapup_persist_shape("", &[]);
        assert!(!msg["content"].as_str().unwrap().trim().is_empty());
    }
}

#[cfg(test)]
mod containment_order_tests {
    use super::*;
    use crate::agent::context::builder::ContextBuilder;
    use crate::agent::context::stats::SessionStats;
    use crate::agent::summarizer_chain::SummarizerChain;
    use crate::config::BigTinyConfig;
    use crate::mcp::MCPManager;

    /// Builds a real `AgentLoop` against an in-memory, migrated DB (same
    /// shape as `agent::mod::tests::test_agent`), so the full
    /// sandbox→HITL→execution ordering in `execute_one_tool_call` runs for
    /// real. Default HITL policy is `always_ask` — exactly the configuration
    /// that registered phantom pending actions before the fix.
    async fn test_loop() -> (AgentLoop, Arc<Mutex<HITLManager>>) {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();

        let config = BigTinyConfig::default();
        let router = Arc::new(ProviderRouter::new(config.cache.clone()));
        let mcp = Arc::new(MCPManager::new(pool.clone(), None));
        let hitl = Arc::new(Mutex::new(HITLManager::new(
            pool.clone(),
            config.hitl.clone(),
        )));
        let summarizer = Arc::new(SummarizerChain::new(
            None,
            router.clone(),
            config.summarizer.clone(),
        ));
        let context = ContextBuilder::new(
            pool.clone(),
            config.token_management.clone(),
            config.summarizer.reserve_exchanges,
        );
        let stats = SessionStats::new(pool.clone());
        let agent_loop = AgentLoop::new(
            router,
            hitl.clone(),
            mcp,
            Arc::new(DashMap::new()),
            context,
            stats,
            summarizer,
            config.summarizer.clone(),
            config.memory.clone(),
            Arc::new(PreflightCounters::new()),
            4,
            std::env::temp_dir().to_string_lossy().into_owned(),
            config.fallback.clone(),
            false,
            crate::plugins::test_plugin_host(&pool),
            config.pathway.clone(),
            Arc::new(DashMap::new()),
            Arc::new(DashMap::new()),
            Arc::new(DashMap::new()),
            pool.clone(),
        );
        (agent_loop, hitl)
    }

    /// Regression: the write-class containment hard-deny used to run AFTER
    /// `check_tool_call_with_rules`, whose `always_ask` side effect had
    /// already registered a pending action — the call was then denied
    /// without a HitlPause, leaving a phantom entry in the
    /// pending-approvals API for ~1h. The containment check now runs first,
    /// so no pending action is ever created for a denied write.
    #[tokio::test]
    async fn write_tool_containment_deny_creates_no_pending_action() {
        let (agent_loop, hitl) = test_loop().await;
        let (tx, _rx) = mpsc::unbounded_channel::<SSEEvent>();
        let result = agent_loop
            .execute_one_tool_call(
                "sess-1",
                "lean_file_write".to_string(),
                json!({"path": "/etc/evil.txt", "content": "x"}),
                &["/allowed".to_string()],
                Arc::new(Semaphore::new(1)),
                &tx,
            )
            .await;
        assert!(
            result.contains("denied"),
            "the out-of-dir write is hard-denied: {result}"
        );
        assert!(
            hitl.lock().await.get_pending_approvals("sess-1").is_empty(),
            "no phantom pending action may be registered"
        );
    }

    /// The mirror case: an in-dir write still reaches the HITL layer
    /// (always_ask → a real pending action with a HitlPause event).
    #[tokio::test]
    async fn write_tool_inside_allowed_dirs_still_goes_through_hitl() {
        let (agent_loop, hitl) = test_loop().await;
        let (tx, mut rx) = mpsc::unbounded_channel::<SSEEvent>();
        let agent_loop = Arc::new(agent_loop);
        let al = agent_loop.clone();
        let handle = tokio::spawn(async move {
            al.execute_one_tool_call(
                "sess-1",
                "lean_file_write".to_string(),
                json!({"path": "/allowed/ok.txt", "content": "x"}),
                &["/allowed".to_string()],
                Arc::new(Semaphore::new(1)),
                &tx,
            )
            .await
        });
        // Wait for the HitlPause, then approve via the manager directly.
        let mut action_id = None;
        while let Some(ev) = rx.recv().await {
            if ev.event_type == SSEEventType::HitlPause {
                action_id = ev.action_id;
                break;
            }
        }
        let action_id = action_id.expect("a HitlPause with an action_id must arrive");
        {
            let mut hitl = hitl.lock().await;
            hitl.record_decision(&action_id, "allow");
        }
        // Wake the paused call the way `Agent::resolve_approval` would.
        if let Some((_, notify)) = agent_loop.hitl_notifies.remove(&action_id) {
            notify.notify_one();
        }
        let result = handle.await.unwrap();
        // The tool itself doesn't exist (no MCP servers registered), but the
        // call must have gotten PAST the HITL gate — an "unknown tool"
        // execution error, not a denial.
        assert!(
            !result.contains("denied"),
            "an approved in-dir write must not be denied: {result}"
        );
    }

    /// #1 regression: a mid-stream transient-error delta (`error_type ==
    /// "request"`, the shape the parsers emit on a dropped connection, idle
    /// timeout, or SSE `error` event) must fail the attempt as a
    /// `ProviderError::Request` so the caller's retry/failover block handles
    /// it — not fall through as an empty `finish_reason:"error"` that
    /// triggered unbounded step-retries.
    #[tokio::test]
    async fn process_stream_surfaces_a_mid_stream_error_delta_as_a_provider_error() {
        let (agent_loop, _hitl) = test_loop().await;
        let (tx, _rx) = mpsc::unbounded_channel::<SSEEvent>();
        let stream: Pin<Box<dyn Stream<Item = Delta> + Send>> =
            Box::pin(futures::stream::iter(vec![Delta {
                role: "assistant".into(),
                content: None,
                reasoning: None,
                tool_calls: None,
                finish_reason: Some("error".into()),
                usage: None,
                error_type: Some("request".into()),
            }]));
        let result = agent_loop.process_stream(stream, &tx).await;
        match result {
            Err(ProviderError::Request { .. }) => {}
            other => panic!("expected ProviderError::Request, got {other:?}"),
        }
    }

    /// #1 regression (mirror): even after content was already streamed, a
    /// trailing transient-error delta must still fail the attempt — partial
    /// content must never be persisted as if the turn succeeded.
    #[tokio::test]
    async fn process_stream_errors_out_even_after_partial_content() {
        let (agent_loop, _hitl) = test_loop().await;
        let (tx, _rx) = mpsc::unbounded_channel::<SSEEvent>();
        let stream: Pin<Box<dyn Stream<Item = Delta> + Send>> =
            Box::pin(futures::stream::iter(vec![
                Delta {
                    role: "assistant".into(),
                    content: Some("partial reply".into()),
                    reasoning: None,
                    tool_calls: None,
                    finish_reason: None,
                    usage: None,
                    error_type: None,
                },
                Delta {
                    role: "assistant".into(),
                    content: None,
                    reasoning: None,
                    tool_calls: None,
                    finish_reason: Some("error".into()),
                    usage: None,
                    error_type: Some("request".into()),
                },
            ]));
        let result = agent_loop.process_stream(stream, &tx).await;
        assert!(result.is_err(), "a mid-stream error must fail the attempt");
    }

    /// A clean stream still succeeds and returns the accumulated chunks.
    #[tokio::test]
    async fn process_stream_returns_ok_for_a_clean_stream() {
        let (agent_loop, _hitl) = test_loop().await;
        let (tx, _rx) = mpsc::unbounded_channel::<SSEEvent>();
        let stream: Pin<Box<dyn Stream<Item = Delta> + Send>> =
            Box::pin(futures::stream::iter(vec![Delta {
                role: "assistant".into(),
                content: Some("hello".into()),
                reasoning: None,
                tool_calls: None,
                finish_reason: Some("stop".into()),
                usage: None,
                error_type: None,
            }]));
        let (content_buf, tool_calls, finish_reason, _usage, _timing) = agent_loop
            .process_stream(stream, &tx)
            .await
            .expect("a clean stream must succeed");
        assert_eq!(content_buf, "hello");
        assert!(tool_calls.is_empty());
        assert_eq!(finish_reason.as_deref(), Some("stop"));
    }
}
