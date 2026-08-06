use std::collections::HashSet;

use regex::Regex;
use serde_json::Map;
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::agent::summarizer::SummarizerClient;
use crate::agent::tokens::{count_messages_tokens, count_text_tokens};
use crate::config::{MemoryConfig, SummarizerConfig, TokenManagementConfig};
use crate::storage::sessions;

/// Sub-lists (dotted paths into the memory-slot JSON) that grow append-only and
/// are deduped on merge and capped on consolidation.
const LIST_SLOT_PATHS: &[&str] = &[
    "decision_rationale",
    "exact_identifiers.files_and_paths",
    "exact_identifiers.symbols_and_types",
    "user_facts_and_entities.personal_details",
    "user_facts_and_entities.named_entities",
];

/// Hard cap on `decision_rationale` regardless of the general list cap — the
/// draft's 10-item budget for decisions, applied deterministically (never by
/// asking the summarizer to hold back).
const DECISION_RATIONALE_CAP: usize = 10;

static MEMORY_SLOTS_SCHEMA: once_cell::sync::Lazy<Value> = once_cell::sync::Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "active_artifacts": {
                "type": "object",
                "additionalProperties": {"type": "string"},
                "description": "Named deliverables kept verbatim (drafts, rubrics, specs). Key = short name, value = verbatim text. Keep to a handful."
            },
            "decision_rationale": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Distinct decisions with their why, not already covered by existing memory. At most ~10."
            },
            "exact_identifiers": {
                "type": "object",
                "properties": {
                    "files_and_paths": {"type": "array", "items": {"type": "string"}},
                    "symbols_and_types": {"type": "array", "items": {"type": "string"}}
                }
            },
            "user_facts_and_entities": {
                "type": "object",
                "properties": {
                    "personal_details": {"type": "array", "items": {"type": "string"}},
                    "named_entities": {"type": "array", "items": {"type": "string"}}
                }
            },
            "current_task_state": {"type": "string"}
        },
        "required": ["active_artifacts", "decision_rationale", "exact_identifiers", "user_facts_and_entities", "current_task_state"]
    })
});

/// Read a (possibly dotted) path out of a slot `Value`, returning `None` for
/// a missing or malformed segment rather than panicking on a non-object hop.
fn path_value<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = root;
    for part in path.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

fn path_strings(root: &Value, path: &str) -> Vec<String> {
    path_value(root, path)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Like [`path_strings`] but reads from a `Map` root without cloning it into a
/// `Value` first (`merge_memory_slots` walks several dotted sub-lists per pass,
/// and `Value::Object(root.clone())` allocated the whole map once per path).
fn map_path_strings(root: &Map<String, Value>, path: &str) -> Vec<String> {
    let mut parts = path.split('.');
    let Some(first) = parts.next() else {
        return Vec::new();
    };
    let Some(mut cur) = root.get(first) else {
        return Vec::new();
    };
    for part in parts {
        match cur.get(part) {
            Some(v) => cur = v,
            None => return Vec::new(),
        }
    }
    cur.as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default()
}

/// Write a list at a (possibly dotted) path, creating intermediate objects.
fn set_path_strings(map: &mut Map<String, Value>, path: &str, items: Vec<String>) {
    let value = Value::Array(items.into_iter().map(|s| json!(s)).collect());
    let parts: Vec<&str> = path.split('.').collect();
    if parts.len() == 1 {
        map.insert(parts[0].to_string(), value);
        return;
    }
    let last = parts[parts.len() - 1];
    let mut cur = map;
    for part in &parts[..parts.len() - 1] {
        let entry = cur.entry((*part).to_string()).or_insert_with(|| Value::Object(Map::new()));
        match entry.as_object_mut() {
            Some(obj) => cur = obj,
            None => return,
        }
    }
    cur.insert(last.to_string(), value);
}

fn array_obj(obj: &Map<String, Value>, key: &str) -> Value {
    obj.get(key)
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()))
}

/// Map a legacy (pre-3-tier) memory-slot shape (`new_constraints` /
/// `new_decisions` / `new_completions` / `current_state`) onto the new shape so
/// existing databases normalize on the next compaction pass, and so the renderer
/// and merge only ever have to reason about the new shape. Approximate by
/// design — the legacy keys were coarser buckets — and lossy (`new_completions`,
/// "tasks done", has no new-shape home; those exchanges remain searchable in
/// FTS5). Anything already in the new shape is passed through untouched.
fn normalize_slots(input: &Value) -> Value {
    let Some(obj) = input.as_object() else {
        return Value::Object(Map::new());
    };
    if !(obj.contains_key("new_decisions")
        || obj.contains_key("new_constraints")
        || obj.contains_key("new_completions"))
    {
        return input.clone();
    }

    let mut user_facts = Map::new();
    user_facts.insert(
        "personal_details".to_string(),
        array_obj(obj, "new_constraints"),
    );
    user_facts.insert("named_entities".to_string(), Value::Array(Vec::new()));
    let mut exact = Map::new();
    exact.insert("files_and_paths".to_string(), Value::Array(Vec::new()));
    exact.insert("symbols_and_types".to_string(), Value::Array(Vec::new()));

    let mut out = Map::new();
    out.insert(
        "decision_rationale".to_string(),
        array_obj(obj, "new_decisions"),
    );
    out.insert("user_facts_and_entities".to_string(), Value::Object(user_facts));
    out.insert("exact_identifiers".to_string(), Value::Object(exact));
    out.insert("active_artifacts".to_string(), Value::Object(Map::new()));
    out.insert(
        "current_task_state".to_string(),
        obj.get("current_state")
            .cloned()
            .unwrap_or_else(|| json!("")),
    );
    Value::Object(out)
}

fn append_unique(existing: &[String], incoming: &[String]) -> Vec<String> {
    let mut seen: HashSet<String> =
        existing.iter().map(|s| s.trim().to_lowercase()).collect();
    let mut out = existing.to_vec();
    for s in incoming {
        let t = s.trim().to_string();
        let lower = t.to_lowercase();
        if !t.is_empty() && !seen.contains(&lower) {
            seen.insert(lower);
            out.push(t);
        }
    }
    out
}

/// Renders persisted memory slots as the `[CONSOLIDATED PROJECT MEMORY]` system
/// block — a *deterministic* JSON->markdown conversion (the summarizer writes
/// the predefined `MEMORY_SLOTS_SCHEMA` template; this function does the
/// rendering, never the model). Legacy-shaped slots are normalized first so the
/// renderer only handles the current shape.
pub fn render_memory_block(slots: Option<&Value>) -> Option<String> {
    let slots = normalize_slots(slots?);
    let obj = slots.as_object()?;
    let mut lines = vec!["[CONSOLIDATED PROJECT MEMORY]".to_string()];
    let mut has_content = false;

    if let Some(art) = obj.get("active_artifacts").and_then(|v| v.as_object()) {
        for (k, v) in art {
            if v.as_str().map(|s| s.trim().is_empty()).unwrap_or(true) {
                continue;
            }
            has_content = true;
            lines.push(format!("## Active Artifact: {k}"));
            lines.push(v.as_str().unwrap_or("").to_string());
        }
    }

    let mut list_block = |label: &str, items: &[String]| {
        if items.is_empty() {
            return Vec::new();
        }
        has_content = true;
        let mut out = vec![format!("## {label}")];
        for it in items {
            out.push(format!("- {it}"));
        }
        out
    };

    lines.extend(list_block(
        "Decision Rationale",
        &path_strings(&slots, "decision_rationale"),
    ));

    let ident = vec![
        (
            "files_and_paths",
            path_strings(&slots, "exact_identifiers.files_and_paths"),
        ),
        (
            "symbols_and_types",
            path_strings(&slots, "exact_identifiers.symbols_and_types"),
        ),
    ]
    .into_iter()
    .filter(|(_, v)| !v.is_empty())
    .collect::<Vec<_>>();
    if !ident.is_empty() {
        has_content = true;
        lines.push("## Exact Identifiers".to_string());
        for (sub, v) in ident {
            lines.push(format!("- {sub}: {}", v.join(", ")));
        }
    }

    let facts = vec![
        (
            "personal",
            path_strings(&slots, "user_facts_and_entities.personal_details"),
        ),
        (
            "entities",
            path_strings(&slots, "user_facts_and_entities.named_entities"),
        ),
    ]
    .into_iter()
    .filter(|(_, v)| !v.is_empty())
    .collect::<Vec<_>>();
    if !facts.is_empty() {
        has_content = true;
        lines.push("## User Facts & Entities".to_string());
        for (sub, v) in facts {
            lines.push(format!("- {sub}: {}", v.join(", ")));
        }
    }

    if let Some(state) = path_value(&slots, "current_task_state").and_then(|v| v.as_str()) {
        if !state.trim().is_empty() {
            has_content = true;
            lines.push(format!("## Current Task State\n{state}"));
        }
    }

    if !has_content {
        return None;
    }

    Some(lines.join("\n"))
}

/// Append-only merge of a summarizer pass into existing memory slots. Emits
/// the *current* (3-tier) shape: legacy inputs are normalized first. List slots
/// grow deduped (never rewritten); `active_artifacts` merges keyed maps
/// newest-wins-per-key; `current_task_state` is last-write-wins.
pub fn merge_memory_slots(existing: Option<&Value>, new: &Value) -> Value {
    // Defensive: a misbehaving/older model can return a valid-but-wrong-shaped
    // JSON value (bare string, array, null...). Treating that as "no new slots
    // this pass" (falls through to what `existing` had) is the safe
    // degradation — see the pre-existing guard this preserves.
    let old = existing
        .map(normalize_slots)
        .unwrap_or_else(|| Value::Object(Map::new()));
    let new_norm = normalize_slots(new);
    let mut out = match old.as_object() {
        Some(m) => m.clone(),
        None => Map::new(),
    };
    let new_obj = match new_norm.as_object() {
        Some(m) => m.clone(),
        None => return Value::Object(out),
    };

    for path in LIST_SLOT_PATHS {
        let merged = append_unique(&map_path_strings(&out, path), &path_strings(&new_norm, path));
        set_path_strings(&mut out, path, merged);
    }

    if let Some(new_art) = new_obj.get("active_artifacts").and_then(|v| v.as_object()) {
        let entry = out
            .entry("active_artifacts".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(art) = entry.as_object_mut() {
            for (k, v) in new_art {
                art.insert(k.clone(), v.clone());
            }
        }
    }

    if let Some(state) = new_obj.get("current_task_state").and_then(|v| v.as_str()) {
        if !state.trim().is_empty() {
            out.insert("current_task_state".to_string(), json!(state));
        }
    }

    Value::Object(out)
}

/// Bounded, deterministic shrink. `decision_rationale` is hard-capped at 10;
/// the identifier/fact sub-lists at `max_items`; `active_artifacts` is kept
/// under `artifacts_max_tokens` by dropping keys oldest-first (insertion order,
/// courtesy of serde_json `preserve_order`). Survivors are kept verbatim — the
/// full evicted text remains searchable in FTS5.
pub fn consolidate_slot_if_needed(mut slots: Value, max_items: i32, artifacts_max_tokens: i32) -> Value {
    let max_items = max_items.max(0) as usize;

    if let Some(obj) = slots.as_object_mut() {
        if let Some(arr) = obj.get_mut("decision_rationale").and_then(|v| v.as_array_mut()) {
            if arr.len() > DECISION_RATIONALE_CAP {
                *arr = arr.split_off(arr.len() - DECISION_RATIONALE_CAP);
            }
        }
    }

    for path in &LIST_SLOT_PATHS[1..] {
        if let Some(Value::Array(arr)) = path_mut(&mut slots, path) {
            if arr.len() > max_items {
                *arr = arr.split_off(arr.len() - max_items);
            }
        }
    }

    if let Some(art) = slots.as_object_mut().and_then(|o| o.get_mut("active_artifacts")).and_then(|v| v.as_object_mut()) {
        if artifacts_max_tokens > 0 {
            loop {
                let total: i32 = art.values().map(|v| count_text_tokens(v.as_str().unwrap_or(""))).sum();
                if total <= artifacts_max_tokens || art.is_empty() {
                    break;
                }
                // Drop the oldest (first-inserted) key.
                let oldest = art.keys().next().cloned();
                if let Some(k) = oldest {
                    art.shift_remove(&k);
                } else {
                    break;
                }
            }
        }
    }

    slots
}

fn path_mut<'a>(root: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    let mut cur = root;
    for part in path.split('.') {
        cur = cur.get_mut(part)?;
    }
    Some(cur)
}

// `(?s)` (DOTALL) so `.` crosses newlines — without it this only ever
// matched a fenced block whose body was a single line, since Rust's regex
// crate (like most engines) doesn't match `\n` with `.` by default.
// `[^\n]*` after the opening fence allows an optional language tag
// (` ```rust`, ` ```python`, …) — real code blocks are almost always
// tagged, so the untagged-only version this replaced silently no-op'd on
// exactly the content it exists to elide.
static FENCE_RE: once_cell::sync::Lazy<Regex> =
    once_cell::sync::Lazy::new(|| Regex::new(r"(?s)```[^\n]*\n.*?\n```").unwrap());

fn mask_code_block(fence_block: &str, head_lines: i32, tail_lines: i32) -> String {
    // Clamp to >= 0 at the use site too (config sanitization at load in
    // `config.rs` is the first line of defense): a negative `head_lines`/
    // `tail_lines` used to become a huge `usize` index via `as usize`
    // (`body[..huge..]` panics on a slice out of bounds), and each is
    // additionally clamped to the body length so `body.len() - tail` can
    // never underflow.
    let head = head_lines.max(0) as usize;
    let tail = tail_lines.max(0) as usize;

    let lines: Vec<&str> = fence_block.lines().collect();
    if lines.len() < 2 {
        return fence_block.to_string();
    }
    let opening = lines[0];
    let closing = lines[lines.len() - 1];
    let body = &lines[1..lines.len() - 1];

    let head = head.min(body.len());
    let tail = tail.min(body.len());

    if body.len() <= (head + tail) {
        return fence_block.to_string();
    }

    let elided = body.len() - (head + tail);
    let mut kept: Vec<String> = Vec::new();
    kept.extend(body[..head].iter().map(|s| s.to_string()));
    kept.push(format!("[...{elided} lines elided...]"));
    if tail > 0 {
        kept.extend(
            body[body.len() - tail..]
                .iter()
                .map(|s| s.to_string()),
        );
    }

    format!("{opening}\n{}\n{closing}", kept.join("\n"))
}

/// Tier 1: deterministic tool-output elision.
/// Nearest char boundary at or before `idx` — `content.is_char_boundary`
/// makes this stable-Rust-safe (no need for the nightly
/// `floor_char_boundary` API).
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let mut idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Nearest char boundary at or after `idx`.
fn ceil_char_boundary(s: &str, idx: usize) -> usize {
    let mut idx = idx.min(s.len());
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

pub fn apply_tool_mask(
    messages: &[Value],
    reserve_floor_rowid: i64,
    cfg: &TokenManagementConfig,
) -> Vec<Value> {
    let head = cfg.tool_mask_head as usize;
    let tail = cfg.tool_mask_tail as usize;
    let mut out = Vec::new();

    for msg in messages {
        let role = msg.get("role").and_then(|v| v.as_str());
        let rowid = msg.get("rowid").and_then(|v| v.as_i64());

        if role == Some("tool") && rowid.is_some() && rowid.unwrap() < reserve_floor_rowid {
            if let Some(content) = msg.get("content").and_then(|v| v.as_str()) {
                if content.len() > head + tail {
                    // `&content[..head]`/`&content[content.len()-tail..]`
                    // would slice at raw byte offsets and panic whenever a
                    // multi-byte UTF-8 character straddles the boundary —
                    // near-guaranteed for any tool output containing
                    // non-ASCII text at these default 400-byte thresholds.
                    // Round to the nearest valid char boundary instead.
                    let head_idx = floor_char_boundary(content, head);
                    let tail_idx = ceil_char_boundary(content, content.len() - tail);
                    if head_idx < tail_idx {
                        let mut masked = msg.clone();
                        let elided = tail_idx - head_idx;
                        let masked_content = format!(
                            "{}\n[...{} bytes elided; re-run the tool if you need the full output...]\n{}",
                            &content[..head_idx],
                            elided,
                            &content[tail_idx..]
                        );
                        if let Some(obj) = masked.as_object_mut() {
                            obj.insert("content".to_string(), json!(masked_content));
                        }
                        out.push(masked);
                        continue;
                    }
                }
            }
        }
        out.push(msg.clone());
    }

    out
}

/// Tier 1: deterministic masking of large fenced code blocks.
pub fn apply_content_mask(
    messages: &[Value],
    reserve_floor_rowid: i64,
    cfg: &TokenManagementConfig,
) -> Vec<Value> {
    let head = cfg.message_mask_head_lines;
    let tail = cfg.message_mask_tail_lines;
    let mut out = Vec::new();

    for msg in messages {
        let role = msg.get("role").and_then(|v| v.as_str());
        let rowid = msg.get("rowid").and_then(|v| v.as_i64());

        if role != Some("user") && role != Some("assistant") {
            out.push(msg.clone());
            continue;
        }
        if rowid.is_none() || rowid.unwrap() >= reserve_floor_rowid {
            out.push(msg.clone());
            continue;
        }

        let content = match msg.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                out.push(msg.clone());
                continue;
            }
        };

        if !content.contains("```") {
            out.push(msg.clone());
            continue;
        }

        let masked_content = FENCE_RE
            .replace_all(content, |caps: &regex::Captures| {
                mask_code_block(&caps[0], head, tail)
            })
            .to_string();

        if masked_content == content {
            out.push(msg.clone());
        } else {
            let mut masked = msg.clone();
            if let Some(obj) = masked.as_object_mut() {
                obj.insert("content".to_string(), json!(masked_content));
            }
            out.push(masked);
        }
    }

    out
}

/// Groups rows into exchanges: each exchange starts at a `role="user"` message.
pub fn group_into_exchanges(rows: &[Value]) -> Vec<Vec<Value>> {
    let mut exchanges: Vec<Vec<Value>> = Vec::new();
    let mut current = Vec::new();

    for row in rows {
        if row.get("role").and_then(|v| v.as_str()) == Some("user") && !current.is_empty() {
            exchanges.push(current);
            current = Vec::new();
        }
        current.push(row.clone());
    }
    if !current.is_empty() {
        exchanges.push(current);
    }

    exchanges
}

/// Returns the rowid of the first message in the reserved live tail.
///
/// A `reserve_exchanges <= 0` or one that covers every exchange historically
/// fell into `&exchanges[len - reserve..]`, which is an empty slice for
/// `reserve == 0` and panicked on `reserved[0][0]` — a `SummarizerConfig`
/// with `reserve_exchanges: 0` (or a session with very few exchanges) could
/// kill an otherwise-healthy compaction pass. Both degenerate cases now
/// return the earliest rowid (nothing is excluded from folding).
pub fn find_reserve_floor_rowid(rows: &[Value], reserve_exchanges: i32) -> i64 {
    let exchanges = group_into_exchanges(rows);
    let earliest = || {
        rows.first()
            .and_then(|r| r.get("rowid").and_then(|v| v.as_i64()))
            .unwrap_or(0)
    };
    if reserve_exchanges <= 0 || exchanges.len() <= reserve_exchanges as usize {
        return earliest();
    }

    let reserved = &exchanges[exchanges.len() - reserve_exchanges as usize..];
    reserved[0][0]
        .get("rowid")
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
}

/// Synchronous emergency trim: drop whole exchanges from eligible region.
pub fn emergency_trim(
    messages: &[Value],
    reserve_floor_rowid: i64,
    target_tokens: i32,
) -> Vec<Value> {
    let eligible: Vec<Value> = messages
        .iter()
        .filter(|m| {
            m.get("rowid")
                .and_then(|v| v.as_i64())
                .map(|r| r < reserve_floor_rowid)
                .unwrap_or(false)
        })
        .cloned()
        .collect();

    let reserved: Vec<Value> = messages
        .iter()
        .filter(|m| {
            !m.get("rowid")
                .and_then(|v| v.as_i64())
                .map(|r| r < reserve_floor_rowid)
                .unwrap_or(false)
        })
        .cloned()
        .collect();

    if eligible.is_empty() {
        return messages.to_vec();
    }

    let mut exchanges = group_into_exchanges(&eligible);
    let mut total = count_messages_tokens(messages);
    let mut dropped_count = 0;

    while total > target_tokens && !exchanges.is_empty() {
        let victim = exchanges.remove(0);
        total -= count_messages_tokens(&victim);
        dropped_count += victim.len();
    }

    let mut result: Vec<Value> = Vec::new();
    if dropped_count > 0 {
        result.push(json!({
            "role": "system",
            "content": format!("[{} earlier tool interactions elided to fit context]", dropped_count)
        }));
    }

    for exchange in exchanges {
        result.extend(exchange);
    }
    result.extend(reserved);

    result
}

#[derive(Debug, Clone)]
pub struct CompactionResult {
    pub messages_compacted: usize,
    pub tokens_before: i32,
    pub tokens_after: i32,
}

/// Guidance for the summarizer model. The actual schema (`MEMORY_SLOTS_SCHEMA`)
/// constrains the output shape; this prose steers *content* (only new items,
/// no restatement) and describes what each field is for.
const SUMMARIZER_INSTRUCTIONS: &str =
    "You are compacting an AI coding assistant's conversation history. You \
     are given EXISTING PROJECT MEMORY (already known) and a NEW CHUNK of \
     conversation. Extract ONLY items from the new chunk that are not \
     already covered by existing memory — do not repeat existing items, do \
     not restate the whole history. active_artifacts holds named deliverables \
     kept verbatim; decision_rationale holds distinct decisions with their why \
     (at most 10, only new ones); exact_identifiers and user_facts_and_entities \
     hold exact paths/symbols and user/entity facts respectively. Set \
     current_task_state to the immediate focus/next-step as of the end of the \
     new chunk. Respond with JSON matching the given schema only.";

/// Build the prompt for the summarizer to extract memory from a chunk.
pub fn build_summarizer_prompt(existing_slots: Option<&Value>, chunk: &[Value]) -> Vec<Value> {
    let existing_block = existing_slots
        .map(|v| v.to_string())
        .unwrap_or_else(|| "(none yet)".to_string());

    let mut chunk_lines = Vec::new();
    for msg in chunk {
        let role = msg
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let content_str = msg
            .get("content")
            .map(|v| {
                if v.is_string() {
                    v.as_str().unwrap().to_string()
                } else {
                    v.to_string()
                }
            })
            .unwrap_or_default();

        let content_with_tools = if let Some(tc) = msg.get("tool_calls") {
            format!("{content_str} [tool_calls: {}]", tc)
        } else {
            content_str
        };

        chunk_lines.push(format!("{role}: {content_with_tools}"));
    }

    vec![
        json!({
            "role": "system",
            "content": SUMMARIZER_INSTRUCTIONS
        }),
        json!({
            "role": "user",
            "content": format!(
                "EXISTING PROJECT MEMORY:\n{}\n\nNEW CHUNK:\n{}",
                existing_block,
                chunk_lines.join("\n")
            )
        }),
    ]
}

/// Full compaction pass for one session. Wraps the actual work
/// (`run_compaction_inner`) in a compare-and-swap lock so overlapping
/// triggers for the same session (compaction fires fire-and-forget after
/// every turn) can't race on `compacted_through_rowid`/`memory_slots`;
/// stale locks (left behind by a crashed pass) are reclaimed after
/// `2 * summarizer_cfg.timeout_s`.
///
/// `force` bypasses the automatic token threshold (`total_tokens <=
/// high_water` early-return in `run_compaction_inner`): the manual `/compact`
/// command sets it so a short-but-live session still folds into memory, while
/// the post-turn automatic call always passes `false` and keeps the old
/// budget-gated behavior.
#[allow(clippy::too_many_arguments)]
pub async fn run_compaction(
    pool: &SqlitePool,
    session_id: &str,
    summarizer: &SummarizerClient,
    token_cfg: &TokenManagementConfig,
    summarizer_cfg: &SummarizerConfig,
    memory_cfg: &MemoryConfig,
    context_length: i32,
    force: bool,
) -> Option<CompactionResult> {
    if !summarizer_cfg.enabled {
        return None;
    }

    let stale_after = chrono::Duration::seconds((summarizer_cfg.timeout_s * 2.0).ceil() as i64);
    match sessions::try_acquire_compaction_lock(pool, session_id, stale_after).await {
        Ok(true) => {}
        _ => return None,
    }

    let result = run_compaction_inner(
        pool,
        session_id,
        summarizer,
        token_cfg,
        summarizer_cfg,
        memory_cfg,
        context_length,
        force,
    )
    .await;

    // Always release, whether the pass succeeded, bailed out early, or
    // failed — `update_compaction_state` (success path) already sets
    // compaction_state back to 'idle', so this is a harmless no-op there.
    let _ = sessions::release_compaction_lock(pool, session_id).await;

    result
}

#[allow(clippy::too_many_arguments)]
async fn run_compaction_inner(
    pool: &SqlitePool,
    session_id: &str,
    summarizer: &SummarizerClient,
    token_cfg: &TokenManagementConfig,
    summarizer_cfg: &SummarizerConfig,
    memory_cfg: &MemoryConfig,
    context_length: i32,
    force: bool,
) -> Option<CompactionResult> {
    let session = match sessions::get_session(pool, session_id).await.ok().flatten() {
        Some(s) => s,
        None => return None,
    };

    let compacted_through = session.compacted_through_rowid;
    let existing_slots = session
        .memory_slots
        .as_ref()
        .and_then(|s| serde_json::from_str(s).ok());

    // Fetch messages after compacted_through
    let rows = match crate::storage::messages::get_messages_after_rowid(
        pool,
        session_id,
        compacted_through,
    )
    .await
    {
        Ok(r) => r,
        Err(_) => return None,
    };

    if rows.is_empty() {
        return None;
    }

    // Convert to Value format
    let mut values: Vec<Value> = Vec::new();
    for row in &rows {
        let mut msg = serde_json::Map::new();
        msg.insert("rowid".to_string(), json!(row.rowid));
        msg.insert("role".to_string(), json!(row.role));
        if let Some(ref c) = row.content {
            msg.insert("content".to_string(), json!(c));
        }
        if let Some(ref tc) = row.tool_calls {
            if let Ok(parsed) = serde_json::from_str(tc) {
                msg.insert("tool_calls".to_string(), parsed);
            }
        }
        if let Some(ref tcid) = row.tool_call_id {
            msg.insert("tool_call_id".to_string(), json!(tcid));
        }
        values.push(Value::Object(msg));
    }

    // Remove system messages
    let values: Vec<Value> = values
        .into_iter()
        .filter(|v| v.get("role").and_then(|r| r.as_str()) != Some("system"))
        .collect();

    if values.is_empty() {
        return None;
    }

    let reserve_floor = find_reserve_floor_rowid(&values, summarizer_cfg.reserve_exchanges);
    let candidate_rows: Vec<Value> = values
        .iter()
        .filter(|v| {
            v.get("rowid")
                .and_then(|r| r.as_i64())
                .map(|r| r < reserve_floor)
                .unwrap_or(false)
        })
        .cloned()
        .collect();

    if candidate_rows.is_empty() {
        return None;
    }

    let high_water = token_cfg
        .min_compaction_tokens
        .max((context_length as f64 * token_cfg.compaction_threshold) as i32);

    let low_water = (context_length as f64 * token_cfg.compaction_target_ratio) as i32;

    // Sum only the FOLDABLE rows' tokens, not `rows.iter()` as a whole. The
    // old `total_tokens` included the reserved live tail (never foldable) and
    // the system rows already dropped from `values`, inflating the fold
    // region's real size — the trigger fired earlier than the foldable
    // region warranted and the fold loop started from that inflated total,
    // stopping high above low-water. Base both the trigger and the fold
    // budget on the candidate region only.
    let token_of = |rowid: i64| -> i64 {
        rows.iter()
            .find(|r| r.rowid == rowid)
            .map(|r| r.token_count.unwrap_or(0) as i64)
            .unwrap_or(0)
    };
    let foldable_tokens: i64 = candidate_rows
        .iter()
        .filter_map(|v| v.get("rowid").and_then(|r| r.as_i64()))
        .map(&token_of)
        .sum();

    if !force && foldable_tokens <= high_water as i64 {
        return None;
    }

    let candidate_exchanges = group_into_exchanges(&candidate_rows);
    let mut to_fold: Vec<Value> = Vec::new();
    let mut remaining_tokens = foldable_tokens;

    // Calculate per-exchange token count
    for exchange in &candidate_exchanges {
        let exchange_tokens: i64 = exchange
            .iter()
            .map(|v| token_of(v.get("rowid").and_then(|r| r.as_i64()).unwrap_or(0)))
            .sum();

        to_fold.extend(exchange.clone());
        remaining_tokens -= exchange_tokens;

        if remaining_tokens <= low_water as i64 {
            break;
        }
    }

    if to_fold.is_empty() {
        return None;
    }

    // Apply masking to tool outputs in the fold region
    let masked = apply_tool_mask(&to_fold, reserve_floor, token_cfg);
    let prompt = build_summarizer_prompt(existing_slots.as_ref(), &masked);

    let new_slots = match summarizer
        .structured_chat(prompt, &MEMORY_SLOTS_SCHEMA)
        .await
    {
        Ok(slots) => slots,
        Err(e) => {
            // Never fail the turn or corrupt state on a bad summarizer pass —
            // just skip this compaction attempt, matching Python's
            // `except SummarizerError` handling.
            tracing::warn!("compaction: summarizer call failed for session {session_id}: {e}");
            return None;
        }
    };

    let merged = merge_memory_slots(existing_slots.as_ref(), &new_slots);
    let merged =
        consolidate_slot_if_needed(merged, summarizer_cfg.max_slot_items, memory_cfg.artifacts_max_tokens);

    let new_watermark = to_fold
        .last()
        .and_then(|v| v.get("rowid").and_then(|r| r.as_i64()))
        .unwrap_or(compacted_through);

    let slots_json = serde_json::to_string(&merged).ok()?;
    if let Err(e) =
        sessions::update_compaction_state(pool, session_id, &slots_json, new_watermark).await
    {
        tracing::error!("compaction: failed to update session: {}", e);
        return None;
    }

    let tokens_folded: i64 = to_fold
        .iter()
        .map(|v| token_of(v.get("rowid").and_then(|r| r.as_i64()).unwrap_or(0)))
        .sum();
    let tokens_after = (foldable_tokens - tokens_folded) as i32;

    Some(CompactionResult {
        messages_compacted: to_fold.len(),
        tokens_before: foldable_tokens as i32,
        tokens_after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_memory_block_empty() {
        assert!(render_memory_block(None).is_none());
    }

    #[test]
    fn fence_re_matches_language_tagged_multiline_blocks() {
        // Regression for the two compounding bugs in the old
        // ```\n.*?\n``` pattern: no DOTALL (so `.` never crossed the
        // newlines inside a real multi-line body) and no allowance for a
        // language tag after the opening fence (```rust, ```python, …).
        let body: String = (0..20).map(|i| format!("line {i}\n")).collect();
        let content = format!("before\n```rust\n{body}```\nafter");
        let cfg = TokenManagementConfig {
            message_mask_head_lines: 2,
            message_mask_tail_lines: 2,
            ..Default::default()
        };
        let messages = vec![json!({
            "role": "assistant",
            "content": content,
            "rowid": 1,
        })];
        let masked = apply_content_mask(&messages, 100, &cfg);
        let masked_content = masked[0]["content"].as_str().unwrap();
        assert!(masked_content.contains("elided"));
        assert!(masked_content.contains("```rust"));
        assert!(masked_content.starts_with("before\n"));
        assert!(masked_content.ends_with("after"));
    }

    #[test]
    fn test_render_memory_block_with_content() {
        let slots = json!({
            "active_artifacts": {"assignment_description": "3-part digital cultures essay."},
            "decision_rationale": ["Use SQLite FTS5 for latency"],
            "exact_identifiers": {"files_and_paths": ["src/db.rs"], "symbols_and_types": ["run_compaction_inner"]},
            "user_facts_and_entities": {"personal_details": ["Instructor in Philadelphia"], "named_entities": ["Haverford"]},
            "current_task_state": "Setting up peer review rubric"
        });
        let block = render_memory_block(Some(&slots));
        assert!(block.is_some());
        let block = block.unwrap();
        assert!(block.contains("CONSOLIDATED PROJECT MEMORY"));
        assert!(block.contains("assignment_description"));
        assert!(block.contains("digital cultures essay"));
        assert!(block.contains("Use SQLite FTS5"));
        assert!(block.contains("src/db.rs"));
        assert!(block.contains("Instructor in Philadelphia"));
        assert!(block.contains("peer review rubric"));
    }

    /// Existing databases carry the *legacy* (pre-3-tier) memory slot shape.
    /// The renderer must normalize it rather than render nothing, until the
    /// next compaction pass rewrites it into the new shape.
    #[test]
    fn test_render_memory_block_normalizes_legacy_shape() {
        let slots = json!({
            "new_constraints": ["Use Rust"],
            "new_decisions": ["Use async"],
            "new_completions": ["Wrote main.rs"],
            "current_state": "Implementing tests"
        });
        let block = render_memory_block(Some(&slots));
        assert!(block.is_some());
        let block = block.unwrap();
        assert!(block.contains("Use Rust"));
        assert!(block.contains("Use async"));
        assert!(block.contains("Implementing tests"));
    }

    #[test]
    fn test_merge_memory_slots_append_only() {
        let existing = json!({
            "active_artifacts": {"draft": "v1"},
            "decision_rationale": ["Use async"],
            "exact_identifiers": {"files_and_paths": ["a.rs"]},
            "user_facts_and_entities": {"named_entities": ["Kitty"]},
            "current_task_state": "steady"
        });
        let new = json!({
            "active_artifacts": {"draft": "v2", "rubric": "R"},
            "decision_rationale": ["Use async", "Use sync"],
            "exact_identifiers": {"files_and_paths": ["b.rs"]},
            "user_facts_and_entities": {"named_entities": ["Kitty", "BigTiny"]},
            "current_task_state": "new focus"
        });
        let merged = merge_memory_slots(Some(&existing), &new);
        // decision_rationale dedups, keeps order.
        let decisions = merged.get("decision_rationale").unwrap().as_array().unwrap();
        assert_eq!(decisions.len(), 2);
        assert_eq!(decisions[0], "Use async");
        assert_eq!(decisions[1], "Use sync");
        // named_entities dedup set.
        let entities = merged
            .get("user_facts_and_entities")
            .unwrap()
            .get("named_entities")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(entities.len(), 2);
        // active_artifacts: draft newest-wins, rubric appended.
        let artifacts = merged.get("active_artifacts").unwrap();
        assert_eq!(artifacts.get("draft").unwrap(), "v2");
        assert_eq!(artifacts.get("rubric").unwrap(), "R");
        // current_task_state last-write-wins.
        assert_eq!(merged.get("current_task_state").unwrap(), "new focus");
    }

    /// Legacy input normalizes on merge (so a DB the summarizer last wrote
    /// to pre-upgrade still folds forward into the new shape).
    #[test]
    fn test_merge_normalizes_legacy_input() {
        let existing = json!({"new_decisions": ["Use async"], "current_state": "x"});
        let new = json!({
            "decision_rationale": ["Use sync"],
            "active_artifacts": {},
            "exact_identifiers": {},
            "user_facts_and_entities": {},
            "current_task_state": "y"
        });
        let merged = merge_memory_slots(Some(&existing), &new);
        assert_eq!(merged.get("current_task_state").unwrap(), "y");
        assert_eq!(merged.get("active_artifacts").unwrap().as_object().unwrap().len(), 0);
        // legacy decisions folded into the new list, then appended.
        let decisions = merged.get("decision_rationale").unwrap().as_array().unwrap();
        assert_eq!(decisions.len(), 2);
    }

    #[test]
    fn test_merge_memory_slots_non_object_new_does_not_panic() {
        let existing = json!({
            "decision_rationale": ["Use async"],
            "active_artifacts": {},
            "exact_identifiers": {},
            "user_facts_and_entities": {},
            "current_task_state": "steady"
        });
        // A misbehaving summarizer returning something JSON-valid but not
        // the expected object shape must not panic — it should just
        // contribute nothing this pass, leaving `existing` intact.
        for malformed in [json!("oops"), json!([1, 2, 3]), json!(null), json!(42)] {
            let merged = merge_memory_slots(Some(&existing), &malformed);
            let decisions = merged.get("decision_rationale").unwrap().as_array().unwrap();
            assert_eq!(decisions.len(), 1);
            assert_eq!(decisions[0], "Use async");
        }
    }

    #[test]
    fn test_group_into_exchanges() {
        let rows = vec![
            json!({"role": "user", "content": "Hello", "rowid": 1}),
            json!({"role": "assistant", "content": "Hi", "rowid": 2}),
            json!({"role": "user", "content": "How are you?", "rowid": 3}),
            json!({"role": "assistant", "content": "Fine", "rowid": 4}),
        ];
        let exchanges = group_into_exchanges(&rows);
        assert_eq!(exchanges.len(), 2);
        assert_eq!(exchanges[0].len(), 2);
        assert_eq!(exchanges[1].len(), 2);
    }

    #[test]
    fn test_find_reserve_floor_rowid() {
        let rows = vec![
            json!({"role": "user", "content": "1", "rowid": 1}),
            json!({"role": "assistant", "content": "a", "rowid": 2}),
            json!({"role": "user", "content": "2", "rowid": 3}),
            json!({"role": "assistant", "content": "b", "rowid": 4}),
            json!({"role": "user", "content": "3", "rowid": 5}),
            json!({"role": "assistant", "content": "c", "rowid": 6}),
            json!({"role": "user", "content": "4", "rowid": 7}),
            json!({"role": "assistant", "content": "d", "rowid": 8}),
        ];
        let floor = find_reserve_floor_rowid(&rows, 2);
        assert_eq!(floor, 5); // Reserve last 2 exchanges (rowids 5,6,7,8)
    }

    #[test]
    fn test_consolidate_slot_if_needed() {
        let mut slots = serde_json::Map::new();
        slots.insert(
            "decision_rationale".to_string(),
            json!(["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k"]),
        );
        let slots = Value::Object(slots);
        // Hard 10-cap regardless of the general max_items (which is higher).
        let consolidated = consolidate_slot_if_needed(slots.clone(), 20, 1000);
        let decisions = consolidated
            .get("decision_rationale")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(decisions.len(), 10);
        assert_eq!(decisions[0].as_str().unwrap(), "b");
    }

    #[test]
    fn test_consolidate_evicts_oldest_artifacts_to_budget() {
        let mut slots = serde_json::Map::new();
        // Two long artifacts (many tokens) + one short one (a few tokens).
        // Budget 5 keeps only the short one; insertion order (serde_json
        // `preserve_order`) makes "oldest" the first eviction candidate.
        let mut arts = serde_json::Map::new();
        arts.insert("oldest".to_string(), json!("a".repeat(500)));
        arts.insert("middle".to_string(), json!("b".repeat(500)));
        arts.insert("newest".to_string(), json!("short"));
        slots.insert("active_artifacts".to_string(), Value::Object(arts));
        slots.insert("decision_rationale".to_string(), json!([]));
        slots.insert("exact_identifiers".to_string(), json!({}));
        slots.insert("user_facts_and_entities".to_string(), json!({}));
        slots.insert("current_task_state".to_string(), json!(""));
        let consolidated = consolidate_slot_if_needed(Value::Object(slots), 20, 5);
        let arts = consolidated.get("active_artifacts").unwrap().as_object().unwrap();
        assert!(arts.contains_key("newest"), "budget must keep the small newest artifact");
        assert!(!arts.contains_key("oldest"), "oldest artifact must be evicted first");
        assert!(!arts.contains_key("middle"));
    }

    #[test]
    fn test_apply_tool_mask_basic() {
        let messages = vec![
            json!({"role": "tool", "content": "short output", "rowid": 1}),
            json!({
                "role": "tool",
                "content": "x".repeat(1000),
                "rowid": 1
            }),
        ];
        let cfg = TokenManagementConfig::default();
        let masked = apply_tool_mask(&messages, 100, &cfg);
        assert_eq!(masked.len(), 2);
        // Short message unchanged
        assert_eq!(
            masked[0].get("content").and_then(|v| v.as_str()),
            Some("short output")
        );
        // Long message masked
        let masked_content = masked[1].get("content").and_then(|v| v.as_str()).unwrap();
        assert!(masked_content.contains("elided"));
    }

    #[test]
    fn test_emergency_trim() {
        let messages = vec![
            json!({"role": "user", "content": "x".repeat(100), "rowid": 1}),
            json!({"role": "assistant", "content": "x".repeat(100), "rowid": 2}),
            json!({"role": "user", "content": "current", "rowid": 9}),
            json!({"role": "assistant", "content": "reply", "rowid": 10}),
        ];
        let trimmed = emergency_trim(&messages, 5, 50);
        assert!(!trimmed.is_empty());
    }

    /// Regression: `reserve_exchanges == 0` used to panic on the empty
    /// `&exchanges[len..]` slice; it must return the earliest rowid instead.
    #[test]
    fn test_find_reserve_floor_rowid_zero_reserve_does_not_panic() {
        let rows = vec![
            json!({"role": "user", "content": "1", "rowid": 10}),
            json!({"role": "assistant", "content": "a", "rowid": 20}),
            json!({"role": "user", "content": "2", "rowid": 30}),
            json!({"role": "assistant", "content": "b", "rowid": 40}),
        ];
        assert_eq!(find_reserve_floor_rowid(&rows, 0), 10);
        assert_eq!(find_reserve_floor_rowid(&rows, -1), 10);
        // Reserving more exchanges than exist is also safe (earliest rowid).
        assert_eq!(find_reserve_floor_rowid(&rows, 99), 10);
    }

    /// Regression: negative `message_mask_head_lines`/`tail_lines` became a
    /// huge `usize` slice index and panicked; `mask_code_block` must clamp.
    #[test]
    fn test_mask_code_block_negative_thresholds_do_not_panic() {
        let block = "```rust\nline1\nline2\nline3\nline4\n```";
        for head in [-10, -1, 0] {
            for tail in [-10, -1, 0] {
                let masked = mask_code_block(block, head, tail);
                assert!(
                    masked.contains("rust"),
                    "opening fence lost for head={head} tail={tail}: {masked}"
                );
                assert!(masked.contains("```"));
            }
        }
    }

    /// Regression: a small body with a huge (positive) tail threshold must
    /// not underflow `body.len() - tail`.
    #[test]
    fn test_mask_code_block_tail_larger_than_body_is_safe() {
        let block = "```\nabc\n```";
        let masked = mask_code_block(block, 0, i32::MAX);
        assert_eq!(masked, block);
    }
}
