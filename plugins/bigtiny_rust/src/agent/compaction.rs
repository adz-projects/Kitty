use std::collections::{HashMap, HashSet};

use regex::Regex;
use serde_json::Map;
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::agent::summarizer_chain::SummarizerChain;
use crate::agent::tokens::{count_message_tokens, count_messages_tokens, count_text_tokens};
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
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
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
        let entry = cur
            .entry((*part).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
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
    out.insert(
        "user_facts_and_entities".to_string(),
        Value::Object(user_facts),
    );
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
    let mut seen: HashSet<String> = existing.iter().map(|s| s.trim().to_lowercase()).collect();
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
        let merged = append_unique(
            &map_path_strings(&out, path),
            &path_strings(&new_norm, path),
        );
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
pub fn consolidate_slot_if_needed(
    mut slots: Value,
    max_items: i32,
    artifacts_max_tokens: i32,
) -> Value {
    let max_items = max_items.max(0) as usize;

    if let Some(obj) = slots.as_object_mut() {
        if let Some(arr) = obj
            .get_mut("decision_rationale")
            .and_then(|v| v.as_array_mut())
        {
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

    if let Some(art) = slots
        .as_object_mut()
        .and_then(|o| o.get_mut("active_artifacts"))
        .and_then(|v| v.as_object_mut())
    {
        if artifacts_max_tokens > 0 {
            loop {
                let total: i32 = art
                    .values()
                    .map(|v| count_text_tokens(v.as_str().unwrap_or("")))
                    .sum();
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
        kept.extend(body[body.len() - tail..].iter().map(|s| s.to_string()));
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

/// Mask the middle out of one tool result, keeping `head`/`tail` bytes.
///
/// `None` when the content is already short enough to leave alone, or when
/// the thresholds overlap so there is nothing in the middle to elide — the
/// caller then keeps the message as-is.
///
/// Factored out of `apply_tool_mask` so the in-turn shrink
/// (`shrink_live_turn`) masks tool output by exactly the same rule. That path
/// works on messages appended during the current turn, which have no `rowid`
/// yet and so cannot use the rowid-keyed entry point.
pub fn mask_tool_content(content: &str, head: usize, tail: usize) -> Option<String> {
    if content.len() <= head + tail {
        return None;
    }
    // `&content[..head]`/`&content[content.len()-tail..]` would slice at raw
    // byte offsets and panic whenever a multi-byte UTF-8 character straddles
    // the boundary — near-guaranteed for any tool output containing non-ASCII
    // text at these default 400-byte thresholds. Round to the nearest valid
    // char boundary instead.
    let head_idx = floor_char_boundary(content, head);
    let tail_idx = ceil_char_boundary(content, content.len() - tail);
    if head_idx >= tail_idx {
        return None;
    }
    Some(format!(
        "{}\n[...{} bytes elided; re-run the tool if you need the full output...]\n{}",
        &content[..head_idx],
        tail_idx - head_idx,
        &content[tail_idx..]
    ))
}

pub fn apply_tool_mask(
    messages: &[Value],
    reserve_floor_rowid: i64,
    cfg: &TokenManagementConfig,
) -> Vec<Value> {
    // Same `.max(0)` clamps `mask_code_block` has: a negative config value
    // (possible via env overrides) becomes a huge `usize` via `as usize`,
    // panicking the slice arithmetic below.
    let head = cfg.tool_mask_head.max(0) as usize;
    let tail = cfg.tool_mask_tail.max(0) as usize;
    let mut out = Vec::new();

    for msg in messages {
        let role = msg.get("role").and_then(|v| v.as_str());
        let rowid = msg.get("rowid").and_then(|v| v.as_i64());

        if role == Some("tool") && rowid.is_some() && rowid.unwrap() < reserve_floor_rowid {
            if let Some(content) = msg.get("content").and_then(|v| v.as_str()) {
                if let Some(masked_content) = mask_tool_content(content, head, tail) {
                    let mut masked = msg.clone();
                    if let Some(obj) = masked.as_object_mut() {
                        obj.insert("content".to_string(), json!(masked_content));
                    }
                    out.push(masked);
                    continue;
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
    find_reserve_floor_rowid_budgeted(rows, reserve_exchanges, 0)
}

/// `find_reserve_floor_rowid`, but the reserved live tail must also *fit* in
/// `budget_tokens`. Pass `0` (or less) to disable the budget and get the
/// historical exchange-only behaviour.
///
/// This exists because the exchange-only rule made long agentic turns
/// structurally uncompactable. `group_into_exchanges` starts an exchange at
/// each `role:"user"` message, so a fifty-step research turn is **one**
/// exchange. With `reserve_exchanges = 3`, any session with three or fewer
/// user turns hit the `exchanges.len() <= reserve` arm, got the earliest
/// rowid, and every shrink path keyed off that floor then had nothing to work
/// on: `run_compaction_inner` found no candidate rows and bailed,
/// `emergency_trim` returned its input untouched, and the tool/content masks
/// masked nothing. Even `/compact` with `force = true` could not help — force
/// bypasses the high-water gate, not an empty candidate set. The session was
/// unrecoverable except by starting a new chat.
///
/// So the reserve now *yields* under pressure instead of disabling folding:
/// drop to fewer exchanges, and when even a single exchange is too big to
/// reserve, fall back to reserving a number of trailing **messages** that
/// fits. Reserving less is always safe — it only ever makes more history
/// eligible for folding.
pub fn find_reserve_floor_rowid_budgeted(
    rows: &[Value],
    reserve_exchanges: i32,
    budget_tokens: i32,
) -> i64 {
    let exchanges = group_into_exchanges(rows);
    let earliest = || {
        rows.first()
            .and_then(|r| r.get("rowid").and_then(|v| v.as_i64()))
            .unwrap_or(0)
    };
    let rowid_of = |v: &Value| v.get("rowid").and_then(|r| r.as_i64()).unwrap_or(0);

    if reserve_exchanges <= 0 {
        return earliest();
    }
    if budget_tokens <= 0 {
        // Unbudgeted: historical behaviour, verbatim.
        if exchanges.len() <= reserve_exchanges as usize {
            return earliest();
        }
        let reserved = &exchanges[exchanges.len() - reserve_exchanges as usize..];
        return rowid_of(&reserved[0][0]);
    }

    // Widest reserve that fits, narrowing toward one exchange.
    let max_n = (reserve_exchanges as usize).min(exchanges.len());
    for n in (1..=max_n).rev() {
        let reserved = &exchanges[exchanges.len() - n..];
        let tokens: i32 = reserved
            .iter()
            .map(|ex| count_messages_tokens(ex))
            .sum::<i32>();
        if tokens <= budget_tokens {
            let floor = rowid_of(&reserved[0][0]);
            // Reserving every exchange means nothing is foldable, which is
            // the state we are trying to escape — only accept it when there
            // genuinely is nothing older to fold.
            if n < exchanges.len() || exchanges.len() == 1 {
                return floor;
            }
            return earliest();
        }
    }

    // Even the final exchange is over budget: reserve trailing *messages*.
    reserve_floor_by_messages(rows, budget_tokens).unwrap_or_else(earliest)
}

/// Walk backwards accumulating tokens until `budget_tokens` is spent, then
/// return the rowid of the first message to keep.
///
/// The boundary is snapped so it can never orphan a `role:"tool"` message
/// from the assistant `tool_calls` message that produced it — an
/// OpenAI-compatible endpoint rejects a `tool_call_id` with no matching call,
/// so a "smaller" history that splits a pair is not smaller, it is a 400.
/// Snapping forward (dropping the orphaned results too) is always safe;
/// snapping backwards would grow the reserved region past its budget.
///
/// Returns `None` when the region cannot be split usefully — a single message
/// over budget has nothing to give, and the caller falls back to the earliest
/// rowid.
fn reserve_floor_by_messages(rows: &[Value], budget_tokens: i32) -> Option<i64> {
    if rows.len() < 2 {
        return None;
    }
    let mut spent = 0i32;
    // Never reserve the whole region: index 0 must stay foldable.
    let mut first_kept = rows.len() - 1;
    for idx in (1..rows.len()).rev() {
        spent = spent.saturating_add(count_message_tokens(&rows[idx]));
        if spent > budget_tokens {
            break;
        }
        first_kept = idx;
    }
    let is_tool = |i: usize| rows[i].get("role").and_then(|r| r.as_str()) == Some("tool");

    // Snap the boundary off an orphaned tool result. Forward first (drop the
    // orphans too, staying within budget); if that runs off the end there is
    // nothing left to retain, so snap backwards instead and reserve the
    // assistant message that owns them. Going slightly over budget beats
    // returning an empty tail — and beats reserving the entire history, which
    // is what the caller falls back to.
    let mut forward = first_kept;
    while forward < rows.len() && is_tool(forward) {
        forward += 1;
    }
    if forward < rows.len() {
        first_kept = forward;
    } else {
        while first_kept > 0 && is_tool(first_kept) {
            first_kept -= 1;
        }
    }
    if first_kept == 0 || first_kept >= rows.len() {
        return None;
    }
    rows[first_kept].get("rowid").and_then(|r| r.as_i64())
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

/// Split messages into tool-call groups: every `role:"tool"` message joins
/// the group opened by the message before it. Splitting anywhere other than a
/// group boundary orphans a `tool_call_id` from the assistant `tool_calls`
/// that produced it, which an OpenAI-compatible endpoint rejects with a 400.
fn group_by_tool_calls(messages: &[Value]) -> Vec<std::ops::Range<usize>> {
    let mut groups: Vec<std::ops::Range<usize>> = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        let is_tool = msg.get("role").and_then(|r| r.as_str()) == Some("tool");
        match groups.last_mut() {
            Some(last) if is_tool => last.end = i + 1,
            _ => groups.push(i..i + 1),
        }
    }
    groups
}

/// Number of trailing groups the in-turn shrink will never touch: the model's
/// most recent call and its results, plus the one before it. Below that the
/// model loses the thread of what it was just doing.
const LIVE_SHRINK_PROTECTED_TAIL_GROUPS: usize = 2;

/// Shrink the messages of a turn *in flight* so the next provider request
/// fits, returning `None` when nothing could be given up.
///
/// This is the piece the wrap-up valve was missing. The valve can stop a turn
/// from growing — it withdraws tools and clamps `max_tokens` — but it then
/// sends the same oversized history one more time, so once the history alone
/// exceeds the window the valve's own request is the one that 400s. Nothing
/// here calls the summarizer or touches the database: it is synchronous
/// arithmetic and slicing on the outgoing array, cheap enough to sit directly
/// in front of `chat_completion`.
///
/// It cannot reuse the rowid-keyed `apply_tool_mask`/`emergency_trim` path,
/// because the messages that actually blow the window are the ones this turn
/// just appended — assistant replies and tool results that have no `rowid`
/// until they are persisted, and which those functions therefore treat as
/// permanently reserved. Eligibility here is positional instead.
///
/// Escalates in two phases, both oldest-first and both stopping the moment the
/// budget is met: mask the tool output in eligible groups, then drop eligible
/// groups whole, leaving a note in their place. `role:"system"` messages, the
/// first group (the anchored user message), and the last
/// `LIVE_SHRINK_PROTECTED_TAIL_GROUPS` groups are never touched.
pub fn shrink_live_turn(
    messages: &[Value],
    budget_tokens: i32,
    cfg: &TokenManagementConfig,
) -> Option<Vec<Value>> {
    if budget_tokens <= 0 || count_messages_tokens(messages) <= budget_tokens {
        return None;
    }
    let groups = group_by_tool_calls(messages);
    if groups.len() <= LIVE_SHRINK_PROTECTED_TAIL_GROUPS + 1 {
        return None;
    }
    // `system` messages are injected instructions (persona, the wrap-up
    // notice) and are small; `user` messages are what the turn is *for*.
    // Neither is ever dropped — only assistant replies and tool results.
    let droppable = |g: &std::ops::Range<usize>| {
        !matches!(
            messages[g.start].get("role").and_then(|r| r.as_str()),
            Some("system") | Some("user")
        )
    };
    let eligible: Vec<std::ops::Range<usize>> = groups
        [1..groups.len() - LIVE_SHRINK_PROTECTED_TAIL_GROUPS]
        .iter()
        .filter(|g| droppable(g))
        .cloned()
        .collect();

    let head = cfg.tool_mask_head.max(0) as usize;
    let tail = cfg.tool_mask_tail.max(0) as usize;
    let mut out = messages.to_vec();
    let mut changed = false;

    // Phase 1 — mask tool output in place. Preserves the shape of the turn
    // (every call still has its result), just not the bulk.
    for group in &eligible {
        if count_messages_tokens(&out) <= budget_tokens {
            break;
        }
        for idx in group.clone() {
            if out[idx].get("role").and_then(|r| r.as_str()) != Some("tool") {
                continue;
            }
            let Some(content) = out[idx].get("content").and_then(|c| c.as_str()) else {
                continue;
            };
            if let Some(masked) = mask_tool_content(content, head, tail) {
                if let Some(obj) = out[idx].as_object_mut() {
                    obj.insert("content".to_string(), json!(masked));
                }
                changed = true;
            }
        }
    }

    // Phase 2 — drop whole groups. Only reached when masking every eligible
    // result still left the request over budget, which means the assistant's
    // own replies are the bulk.
    if count_messages_tokens(&out) > budget_tokens {
        let mut dropped: Vec<usize> = Vec::new();
        for group in &eligible {
            if count_messages_tokens(&out) <= budget_tokens {
                break;
            }
            for idx in group.clone() {
                // A tombstone rather than a removal, so the indices in
                // `eligible` stay valid for the rest of the walk; the nulls
                // are filtered out once at the end.
                out[idx] = Value::Null;
                dropped.push(idx);
            }
            changed = true;
        }
        if !dropped.is_empty() {
            let notice = json!({
                "role": "system",
                "content": format!(
                    "[{} earlier steps of this turn were elided to stay within the \
                     context window. Re-run a tool if you need its output again.]",
                    dropped.len()
                )
            });
            out[dropped[0]] = notice;
            out.retain(|m| !m.is_null());
        }
    }

    // Phase 3 — mask the protected tail too. Reached only when giving up
    // every eligible group still was not enough, which means the most recent
    // results are themselves bigger than the whole window. Masking keeps the
    // turn structurally valid (each call still has its result) where dropping
    // the tail would strand the model with no idea what it just did, so this
    // is the last thing tried and the first thing that is still safe.
    if count_messages_tokens(&out) > budget_tokens {
        for idx in 0..out.len() {
            if out[idx].get("role").and_then(|r| r.as_str()) != Some("tool") {
                continue;
            }
            let masked = out[idx]
                .get("content")
                .and_then(|c| c.as_str())
                .and_then(|c| mask_tool_content(c, head, tail));
            if let Some(masked) = masked {
                if let Some(obj) = out[idx].as_object_mut() {
                    obj.insert("content".to_string(), json!(masked));
                }
                changed = true;
            }
            if count_messages_tokens(&out) <= budget_tokens {
                break;
            }
        }
    }

    changed.then_some(out)
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

/// Render a message `content` value as plain prompt text. Text blocks pass
/// through; every non-text block (base64 image payloads, etc.) collapses
/// into a single `[N image(s) attached]` placeholder so structured content
/// never inlines megabytes of base64 into a prompt.
pub fn render_content_as_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => render_blocks_as_text(blocks),
        other => other.to_string(),
    }
}

/// The stored-row form of `render_content_as_text`: `content_format ==
/// "blocks"` rows carry their block array as a JSON *string* (see
/// `context::builder::save_messages`); anything else is plain text already.
/// An unparseable blocks payload passes through unchanged rather than
/// dropping content.
pub fn stored_content_as_text(content: &str, content_format: Option<&str>) -> String {
    if content_format == Some("blocks") {
        if let Ok(Value::Array(blocks)) = serde_json::from_str::<Value>(content) {
            return render_blocks_as_text(&blocks);
        }
    }
    content.to_string()
}

fn render_blocks_as_text(blocks: &[Value]) -> String {
    let mut texts: Vec<&str> = Vec::new();
    let mut images = 0usize;
    for block in blocks {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    texts.push(t);
                }
            }
            _ => images += 1,
        }
    }
    let mut out = texts.join("\n");
    if images > 0 {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("[{images} image(s) attached]"));
    }
    out
}

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
        // Blocks content (image attachments) collapses to a placeholder —
        // never inline base64 payloads into the summarizer prompt.
        let content_str = msg
            .get("content")
            .map(render_content_as_text)
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
    summarizer: &SummarizerChain,
    provider_id: Option<&str>,
    provider_model: Option<String>,
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
        provider_id,
        provider_model,
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
    summarizer: &SummarizerChain,
    provider_id: Option<&str>,
    provider_model: Option<String>,
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
            // `blocks` rows (image attachments) store base64 payloads —
            // collapse them to a placeholder or the summarizer prompt
            // inherits megabytes of base64 (billed at 256 tokens/image).
            msg.insert(
                "content".to_string(),
                json!(stored_content_as_text(c, row.content_format.as_deref())),
            );
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

    // Budgeted by the fold target: if the reserved tail alone is larger than
    // what compaction is trying to get the whole region down to, reserving it
    // in full makes the pass unable to succeed by construction. Letting the
    // reserve narrow instead is what makes a single long agentic turn
    // compactable at all.
    let reserve_floor = find_reserve_floor_rowid_budgeted(
        &values,
        summarizer_cfg.reserve_exchanges,
        (context_length as f64 * token_cfg.compaction_target_ratio) as i32,
    );
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
    // Indexed once rather than a linear `rows.iter().find(...)` per lookup.
    // This closure is called once per candidate row for the trigger sum and
    // then again per row per exchange in the fold loop below, so the scan made
    // the whole pass O(n²) over the uncompacted region.
    let tokens_by_rowid: HashMap<i64, i64> = rows
        .iter()
        .map(|r| (r.rowid, r.token_count.unwrap_or(0) as i64))
        .collect();
    let token_of = |rowid: i64| -> i64 { tokens_by_rowid.get(&rowid).copied().unwrap_or(0) };
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
        .structured_chat_for_session(provider_id, provider_model, prompt, &MEMORY_SLOTS_SCHEMA)
        .await
    {
        Ok(slots) => slots,
        Err(e) => {
            // Never fail the turn or corrupt state on a bad summarizer pass —
            // just skip this compaction attempt.
            tracing::warn!("compaction: summarizer call failed for session {session_id}: {e}");
            return None;
        }
    };

    let merged = merge_memory_slots(existing_slots.as_ref(), &new_slots);
    let merged = consolidate_slot_if_needed(
        merged,
        summarizer_cfg.max_slot_items,
        memory_cfg.artifacts_max_tokens,
    );

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
        let decisions = merged
            .get("decision_rationale")
            .unwrap()
            .as_array()
            .unwrap();
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
        assert_eq!(
            merged
                .get("active_artifacts")
                .unwrap()
                .as_object()
                .unwrap()
                .len(),
            0
        );
        // legacy decisions folded into the new list, then appended.
        let decisions = merged
            .get("decision_rationale")
            .unwrap()
            .as_array()
            .unwrap();
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
            let decisions = merged
                .get("decision_rationale")
                .unwrap()
                .as_array()
                .unwrap();
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
        let arts = consolidated
            .get("active_artifacts")
            .unwrap()
            .as_object()
            .unwrap();
        assert!(
            arts.contains_key("newest"),
            "budget must keep the small newest artifact"
        );
        assert!(
            !arts.contains_key("oldest"),
            "oldest artifact must be evicted first"
        );
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
    /// The failure that made long research sessions unrecoverable: a
    /// fifty-step agentic turn is ONE exchange (exchanges start at each
    /// `role:"user"` message), so with `reserve_exchanges = 3` the old rule
    /// hit `exchanges.len() <= reserve`, reserved the entire history, and left
    /// every shrink path with nothing eligible — including `/compact
    /// --force`, since force bypasses the high-water gate, not an empty
    /// candidate set. Budgeted, the reserve narrows and the turn folds.
    #[test]
    fn a_single_long_agentic_turn_is_foldable_when_budgeted() {
        let big = "x ".repeat(4000);
        let mut rows = vec![json!({"rowid": 1, "role": "user", "content": "go research this"})];
        for i in 0..8 {
            rows.push(json!({"rowid": 2 + i * 2, "role": "assistant", "content": big}));
            rows.push(json!({"rowid": 3 + i * 2, "role": "tool", "content": big}));
        }
        let earliest = 1;

        // Unbudgeted: the whole thing is reserved, nothing folds.
        assert_eq!(find_reserve_floor_rowid(&rows, 3), earliest);

        // Budgeted: the floor moves up, leaving real candidates below it.
        let floor = find_reserve_floor_rowid_budgeted(&rows, 3, 2000);
        assert!(
            floor > earliest,
            "budgeted floor must leave something foldable, got {floor}"
        );
        let candidates = rows
            .iter()
            .filter(|r| r["rowid"].as_i64().unwrap() < floor)
            .count();
        assert!(candidates > 0, "expected foldable rows below {floor}");
    }

    /// A floor that starts the retained region on a `role:"tool"` message
    /// orphans its `tool_call_id` from the assistant `tool_calls` message that
    /// produced it, and an OpenAI-compatible endpoint 400s on that. A history
    /// that splits a pair is not smaller, it is broken — so the boundary snaps
    /// forward past the orphans.
    #[test]
    fn the_reserve_floor_never_orphans_a_tool_result() {
        let big = "y ".repeat(4000);
        let rows = vec![
            json!({"rowid": 1, "role": "user", "content": "start"}),
            json!({"rowid": 2, "role": "assistant", "content": big}),
            json!({"rowid": 3, "role": "tool", "content": big}),
            json!({"rowid": 4, "role": "tool", "content": big}),
            json!({"rowid": 5, "role": "assistant", "content": "done"}),
        ];
        for budget in [200, 800, 2000, 6000] {
            let floor = find_reserve_floor_rowid_budgeted(&rows, 3, budget);
            let first_kept = rows
                .iter()
                .find(|r| r["rowid"].as_i64().unwrap() >= floor)
                .expect("something must be retained");
            assert_ne!(
                first_kept["role"].as_str(),
                Some("tool"),
                "budget {budget}: retained region starts on an orphaned tool result"
            );
        }
    }

    /// The budget must not make things *worse* than the unbudgeted rule for
    /// ordinary sessions that already fit — a generous budget is a no-op.
    #[test]
    fn a_reserve_that_already_fits_is_left_alone() {
        let rows = vec![
            json!({"rowid": 1, "role": "user", "content": "one"}),
            json!({"rowid": 2, "role": "assistant", "content": "a"}),
            json!({"rowid": 3, "role": "user", "content": "two"}),
            json!({"rowid": 4, "role": "assistant", "content": "b"}),
            json!({"rowid": 5, "role": "user", "content": "three"}),
            json!({"rowid": 6, "role": "assistant", "content": "c"}),
        ];
        assert_eq!(
            find_reserve_floor_rowid_budgeted(&rows, 2, 100_000),
            find_reserve_floor_rowid(&rows, 2),
        );
    }

    /// The gap the wrap-up valve could not close: messages appended during
    /// the current turn have no `rowid` yet, so the rowid-keyed
    /// `apply_tool_mask`/`emergency_trim` treat them as permanently reserved
    /// — and those are exactly the messages that blow the window. The valve
    /// would withdraw tools and then send the same oversized array anyway.
    #[test]
    fn a_turn_over_budget_shrinks_even_with_no_rowids() {
        let cfg = TokenManagementConfig::default();
        let big = "z ".repeat(6000);
        let mut messages = vec![
            json!({"role": "system", "content": "persona"}),
            json!({"role": "user", "content": "research this"}),
        ];
        for i in 0..6 {
            messages.push(json!({
                "role": "assistant",
                "tool_calls": [{"id": format!("c{i}"), "type": "function"}]
            }));
            messages.push(json!({"role": "tool", "content": big, "tool_call_id": format!("c{i}")}));
        }
        let before = count_messages_tokens(&messages);
        let budget = before / 4;

        let shrunk = shrink_live_turn(&messages, budget, &cfg).expect("must shrink");
        let after = count_messages_tokens(&shrunk);
        assert!(after < before, "expected a reduction, {before} -> {after}");
        assert!(
            after <= budget,
            "expected to reach the budget of {budget}, got {after}"
        );
    }

    /// Whatever it gives up, the result must still be a *valid* request: every
    /// surviving `tool_call_id` needs the assistant `tool_calls` message that
    /// produced it, or an OpenAI-compatible endpoint 400s on the orphan — and
    /// a request that 400s is not a smaller request, it is a broken one.
    #[test]
    fn shrinking_never_orphans_a_tool_result() {
        let cfg = TokenManagementConfig::default();
        let big = "q ".repeat(6000);
        let mut messages = vec![json!({"role": "user", "content": "start"})];
        for i in 0..8 {
            messages.push(json!({
                "role": "assistant",
                "tool_calls": [{"id": format!("call{i}"), "type": "function"}]
            }));
            messages.push(json!({
                "role": "tool", "content": big, "tool_call_id": format!("call{i}")
            }));
        }
        for divisor in [2, 4, 8, 20] {
            let budget = count_messages_tokens(&messages) / divisor;
            let Some(shrunk) = shrink_live_turn(&messages, budget, &cfg) else {
                continue;
            };
            let issued: HashSet<String> = shrunk
                .iter()
                .filter_map(|m| m.get("tool_calls").and_then(|t| t.as_array()))
                .flatten()
                .filter_map(|c| c.get("id").and_then(|i| i.as_str()).map(str::to_string))
                .collect();
            for m in &shrunk {
                if let Some(id) = m.get("tool_call_id").and_then(|i| i.as_str()) {
                    assert!(
                        issued.contains(id),
                        "budget 1/{divisor}: tool result {id} lost its call"
                    );
                }
            }
        }
    }

    /// A turn that already fits must be left exactly alone — the shrink is a
    /// last resort, not a routine pass, and `None` is what tells the caller
    /// nothing happened.
    #[test]
    fn a_turn_within_budget_is_not_touched() {
        let cfg = TokenManagementConfig::default();
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi"}),
        ];
        assert!(shrink_live_turn(&messages, 100_000, &cfg).is_none());
        // Nothing eligible (too few groups) is also `None`, not a panic.
        assert!(shrink_live_turn(&messages, 1, &cfg).is_none());
    }

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

    /// Regression: negative `tool_mask_head`/`tool_mask_tail` (reachable via
    /// env overrides, which bypass config sanitization) became huge `usize`
    /// values via `as usize` — `apply_tool_mask` must clamp like
    /// `mask_code_block` does.
    #[test]
    fn test_apply_tool_mask_negative_thresholds_do_not_panic() {
        let messages = vec![json!({
            "role": "tool",
            "content": "x".repeat(1000),
            "rowid": 1
        })];
        let cfg = TokenManagementConfig {
            tool_mask_head: -5,
            tool_mask_tail: -5,
            ..Default::default()
        };
        let masked = apply_tool_mask(&messages, 100, &cfg);
        assert_eq!(masked.len(), 1);
    }

    /// Regression: blocks content (image attachments) used to be stringified
    /// into the summarizer prompt verbatim — megabytes of base64 per image.
    /// Non-text blocks must collapse to a `[N image(s) attached]`
    /// placeholder in both the array and stored-string forms.
    #[test]
    fn test_build_summarizer_prompt_collapses_image_blocks() {
        let base64_payload = "QUJD".repeat(1000);
        let chunk = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "describe this"},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{base64_payload}")}},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
            ]
        })];
        let prompt = build_summarizer_prompt(None, &chunk);
        let text = prompt[1]["content"].as_str().unwrap();
        assert!(text.contains("describe this"));
        assert!(text.contains("[2 image(s) attached]"));
        assert!(
            !text.contains(&base64_payload[..64]),
            "no base64 in the summarizer prompt"
        );
    }

    #[test]
    fn test_stored_content_as_text_handles_blocks_rows() {
        let stored = serde_json::to_string(&json!([
            {"type": "text", "text": "look"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}},
        ]))
        .unwrap();
        assert_eq!(
            stored_content_as_text(&stored, Some("blocks")),
            "look\n[1 image(s) attached]"
        );
        // Plain text rows pass through untouched, as does an unparseable
        // blocks payload (never drop content).
        assert_eq!(stored_content_as_text("hello", Some("text")), "hello");
        assert_eq!(
            stored_content_as_text("not json", Some("blocks")),
            "not json"
        );
    }
}
