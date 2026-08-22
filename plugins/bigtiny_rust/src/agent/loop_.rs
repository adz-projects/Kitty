use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use futures::{Stream, StreamExt};
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

/// One completed streamed attempt: the assembled content chunks, tool calls,
/// finish reason, usage, and timing — or a `ProviderError` (including
/// mid-stream failures, which `process_stream` surfaces the same way as a
/// pre-stream `chat_completion` error so both ride one retry/failover budget).
type TurnStreamResult = Result<
    (
        Vec<String>,
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
fn wrapup_persist_shape(content_chunks: &[String], turn_tool_calls: &[ToolCall]) -> Value {
    let text = content_chunks.join("");
    let content = if text.trim().is_empty() {
        if turn_tool_calls.is_empty() {
            "[No reply: the turn ended at the context limit.]".to_string()
        } else {
            "[The turn ended at the context limit before this step could run.]".to_string()
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

/// Strip prompt preamble wrappers from the first user message for title derivation.
fn strip_prompt_wrappers(text: &str) -> String {
    let re_system = regex::Regex::new(r"^<system>\n.*?\n</system>\n\n").unwrap();
    let re_recipe = regex::Regex::new(r"^<recipe\b[^>]*>\n.*?\n</recipe>\n\n[^\n]*\n\n").unwrap();

    let text = re_recipe.replace(text, "").to_string();
    re_system.replace(&text, "").to_string()
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
    let marker = regex::Regex::new(r"^--- .+ ---$").unwrap();
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
    truncate_title(first_line)
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

/// Ask the summarizer chain for a short (3-6 word) title from this
/// session's recent messages. `None` on any failure — no messages yet,
/// every summarizer leg erroring, or a response with no usable `title`
/// field — so the caller falls back to the naive derivation instead of
/// surfacing an error anywhere a user could see it.
async fn summarizer_title(
    pool: &SqlitePool,
    session_id: &str,
    summarizer: &SummarizerChain,
    provider_id: Option<&str>,
    model: Option<String>,
) -> Option<String> {
    let rows = crate::storage::messages::get_last_messages_by_session(pool, session_id, 8)
        .await
        .ok()?;
    let mut convo: Vec<Value> = rows
        .into_iter()
        .filter(|m| matches!(m.role.as_str(), "user" | "assistant"))
        .filter_map(|m| {
            let content = m.content?;
            // Strip the same leading `--- <label> ---` attachment/paste
            // markers `derive_title`'s naive fallback strips (see its doc
            // comment) — a small/weak model given raw marker text as the
            // most prominent thing in the prompt will happily parrot it
            // back as the "title" despite being told not to (confirmed real
            // report: a title of literally "--- Pasted text --- 130
            // words."). Stripping before the model ever sees it is the
            // actual fix; `sanitize_title` below is only a second line of
            // defense for whatever slips past that.
            let content = if m.role == "user" {
                strip_leading_attachment_markers(&strip_prompt_wrappers(&content))
            } else {
                content
            };
            if content.trim().is_empty() {
                return None;
            }
            Some(json!({ "role": m.role, "content": content }))
        })
        .collect();
    if convo.is_empty() {
        return None;
    }
    let mut prompt = vec![json!({
        "role": "user",
        "content": "Read the conversation below and suggest a short, specific, descriptive \
                     title for it — 3 to 6 words, no surrounding quotes, no trailing \
                     punctuation. Describe what the conversation is actually about, not how \
                     any file or pasted text arrived."
    })];
    prompt.append(&mut convo);

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
    let collapsed = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_title(&collapsed)
}

#[cfg(test)]
mod derive_title_tests {
    use super::{derive_title, sanitize_title, strip_leading_attachment_markers, truncate_title};

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
        assert_eq!(derive_title(msg), "What do these have in common?");
    }

    #[test]
    fn a_message_with_no_markers_is_unaffected() {
        let msg = "How do I center a div?";
        assert_eq!(derive_title(msg), "How do I center a div?");
    }

    #[test]
    fn a_line_that_merely_starts_with_dashes_is_not_treated_as_a_marker() {
        // Only a *whole line* matching `--- ... ---` counts — a message that
        // happens to start with "---" for some other reason (a markdown
        // horizontal rule, a code fence) must not be swallowed.
        let msg = "--- this is not a marker\nbecause it has no closing dashes";
        assert_eq!(derive_title(msg), "--- this is not a marker");
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
    pathway: Option<Arc<adaptive_pathway::engine::PathwayEngine>>,
    /// Pathway learning cadence (`learn_every_n` exchanges).
    pathway_cfg: PathwayConfig,
    /// Sessions already warned about a pinned-provider mismatch (the
    /// `ModelFailover` notice at step 0). Shared with the daemon-lifetime
    /// `Agent` — this loop is rebuilt per turn, so the memory of "we already
    /// told the user" must outlive it. An entry is removed the moment the
    /// pinned provider resolves again, so a *new* mismatch appearance
    /// re-warns.
    provider_mismatch_warned: Arc<DashMap<String, ()>>,
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
        pathway: Option<Arc<adaptive_pathway::engine::PathwayEngine>>,
        pathway_cfg: PathwayConfig,
        provider_mismatch_warned: Arc<DashMap<String, ()>>,
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
            memory_cfg,
            preflight,
            max_concurrent_tool_calls,
            cache_dir,
            fallback_cfg,
            sandbox_strict,
            pathway,
            pathway_cfg,
            provider_mismatch_warned,
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
        // later. Resolved once and reused by the pathway-recall path-pick
        // below, which also needs to know which provider/model is active.
        let resolved_provider_id = self
            .router
            .get_provider_id(effective_provider.as_deref())
            .ok();
        let context_tokens_override = resolved_provider_id
            .as_deref()
            .and_then(|pid| self.router.context_length(pid));

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
        let mut step: i64 = 0;
        // Wrap-up valve state, alongside the other survives-iterations values
        // below. `wrapup_issued` is a belt against re-injecting on a later
        // iteration; the unconditional `break` in the completion block is the
        // braces. Both, deliberately — see that block's comment.
        let mut wrapup_issued = false;
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
            let active_tools: &[ToolDefinition] = if provider_takes_tools {
                active_tools
            } else {
                &[]
            };

            // Progressive budget check
            let mut in_budget_check = false;
            let mut in_wrapup = false;
            let mut tools_for_turn = tools_to_openai_format(active_tools);

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
                // persisted into the transcript) — appended to the outgoing
                // clone here instead.
                let outgoing = match &thought_seed_msg {
                    Some(seed) if step == 0 => {
                        let mut o = messages.clone();
                        o.push(seed.clone());
                        o
                    }
                    _ => messages.clone(),
                };
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
                turn_result;

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
                    messages.push(build_assistant_message(&content_chunks, &turn_tool_calls));
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
                messages.push(wrapup_persist_shape(&content_chunks, &turn_tool_calls));
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
            let pool = pool.clone();
            let session_id = session_id.to_string();
            let summarizer = self.summarizer.clone();
            let token_cfg = self.context.config().clone();
            let summarizer_cfg = self.summarizer_cfg.clone();
            let memory_cfg = self.memory_cfg.clone();
            tokio::spawn(async move {
                let _ = run_compaction(
                    &pool,
                    &session_id,
                    &summarizer,
                    Some(pid.as_str()),
                    last_provider_model,
                    &token_cfg,
                    &summarizer_cfg,
                    &memory_cfg,
                    context_length,
                    false,
                )
                .await;
            });
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
            let summarizer = self.summarizer.clone();
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
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
        let engine = self.pathway.clone();
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
        tokio::spawn(async move {
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
        let Some(engine) = self.pathway.as_ref() else {
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
            Vec<String>,
            Vec<ToolCall>,
            Option<String>,
            Option<Value>,
            TimingResult,
        ),
        ProviderError,
    > {
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

            if let Some(ref content) = delta.content {
                if !content.is_empty() {
                    // Count characters, not bytes: the backstop threshold
                    // (`MAX_TURN_CONTENT_CHARS`) is documented in characters,
                    // and `content.len()` (byte count) inflated multi-byte
                    // UTF-8 ~3x, so a legitimate long CJK/emoji reply could
                    // be cut off early.
                    content_chars += content.chars().count();
                    content_chunks.push(content.clone());
                    token_count += 1;
                    let _ = event_tx.send(SSEEvent {
                        event_type: SSEEventType::LlmDelta,
                        content: Some(content.clone()),
                        ..Default::default()
                    });
                }
            }

            if let Some(ref reasoning) = delta.reasoning {
                if !reasoning.is_empty() {
                    // Reasoning counts toward the same ceiling as content —
                    // a thinking-loop stream is exactly the unbounded-output
                    // case `MAX_TURN_CONTENT_CHARS` exists to catch, and
                    // hosted providers get no max_tokens floor.
                    content_chars += reasoning.chars().count();
                    // ...and toward the same delta count. This is only the
                    // fallback for a provider that reports no usage at all,
                    // but leaving thinking out of it meant a reasoning model
                    // on such a provider had most of its generated output
                    // missing from the measured rate.
                    token_count += 1;
                    let _ = event_tx.send(SSEEvent {
                        event_type: SSEEventType::ReasoningDelta,
                        content: Some(reasoning.clone()),
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
        timing.finalize_rate();

        Ok((content_chunks, tool_calls, finish_reason, usage, timing))
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
            let err = format!(
                "Tool {tool_name} denied: it would write to a path outside this \
                 session's allowed directories"
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
            let rules = hitl_rules::list_rules_by_tool(&rules, &tool_name)
                .await
                .unwrap_or_default();
            let mut hitl = self.hitl.lock().await;
            hitl.check_tool_call_with_rules(session_id, &tool_name, &tool_args, &rules)
        };

        if (decision.action == "proceed" || decision.action == "always_allow")
            && !check_containment(&tool_args, allowed_dirs, self.sandbox_strict)
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
        let tool_args = if crate::mcp::builtin::PATHWAY_TOOLS.contains(&tool_name.as_str()) {
            let mut args = tool_args.clone();
            if let Some(obj) = args.as_object_mut() {
                obj.insert("session_id".to_string(), json!(session_id));
            }
            args
        } else {
            tool_args
        };
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
        let msg = wrapup_persist_shape(&["Here is ".into(), "the answer.".into()], &[]);
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["content"], "Here is the answer.");
    }

    /// A model can emit a tool call despite being offered none. Persisting it
    /// would leave `tool_calls` with no `tool` role following, which is a hard
    /// 400 on the *next* turn's first request — and this branch breaks, so
    /// there is no later iteration that could ever supply the results.
    #[test]
    fn tool_calls_are_stripped_rather_than_persisted_dangling() {
        let msg = wrapup_persist_shape(&["Wrapping up.".into()], &[call("c1"), call("c2")]);
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
        for chunks in [vec![], vec![String::new()], vec!["   ".to_string()]] {
            let msg = wrapup_persist_shape(&chunks, &[call("c1")]);
            assert!(
                !msg["content"].as_str().unwrap().trim().is_empty(),
                "empty content is rejected by several backends: {msg}"
            );
            assert!(msg.get("tool_calls").is_none());
        }
        // ...and with no tool calls either, which is the "model said nothing
        // at all" case.
        let msg = wrapup_persist_shape(&[], &[]);
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
            None,
            config.pathway.clone(),
            Arc::new(DashMap::new()),
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
        let (content_chunks, tool_calls, finish_reason, _usage, _timing) = agent_loop
            .process_stream(stream, &tx)
            .await
            .expect("a clean stream must succeed");
        assert_eq!(content_chunks, vec!["hello".to_string()]);
        assert!(tool_calls.is_empty());
        assert_eq!(finish_reason.as_deref(), Some("stop"));
    }
}
