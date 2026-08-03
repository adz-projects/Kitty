use std::collections::HashMap;

use regex::Regex;
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::agent::summarizer::SummarizerClient;
use crate::agent::tokens::count_messages_tokens;
use crate::config::{SummarizerConfig, TokenManagementConfig};
use crate::storage::sessions;

const MEMORY_SLOT_KEYS: &[&str] = &["new_constraints", "new_decisions", "new_completions"];

static MEMORY_SLOTS_SCHEMA: once_cell::sync::Lazy<Value> = once_cell::sync::Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "new_constraints": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Non-negotiable rules, exact paths, or technical bounds introduced in this chunk that were not already known."
            },
            "new_decisions": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Verified facts, architecture choices, or agreed specs established in this chunk."
            },
            "new_completions": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Tasks completed or code changes implemented in this chunk."
            },
            "current_state": {
                "type": "string",
                "description": "The immediate focus area or next step, as of the end of this chunk."
            }
        },
        "required": ["new_constraints", "new_decisions", "new_completions", "current_state"]
    })
});

const SUMMARIZER_INSTRUCTIONS: &str =
    "You are compacting an AI coding assistant's conversation history. You \
     are given EXISTING PROJECT MEMORY (already known) and a NEW CHUNK of \
     conversation. Extract ONLY items from the new chunk that are not \
     already covered by existing memory — do not repeat existing items, do \
     not restate the whole history. Set current_state to the immediate \
     focus/next-step as of the end of the new chunk. Respond with JSON \
     matching the given schema only.";

/// Renders persisted memory slots as the `[CONSOLIDATED PROJECT MEMORY]` system block.
pub fn render_memory_block(slots: Option<&Value>) -> Option<String> {
    let slots = slots?.as_object()?;
    let mut lines = vec!["[CONSOLIDATED PROJECT MEMORY]".to_string()];

    let labels = [
        ("new_constraints", "User Constraints"),
        ("new_decisions", "Key Decisions"),
        ("new_completions", "Completed Actions"),
    ];

    let mut has_content = false;
    for (key, label) in &labels {
        if let Some(items) = slots.get(*key).and_then(|v| v.as_array()) {
            if !items.is_empty() {
                has_content = true;
                lines.push(format!("- {label}:"));
                for item in items {
                    if let Some(s) = item.as_str() {
                        lines.push(format!("  - {s}"));
                    }
                }
            }
        }
    }

    if let Some(current_state) = slots.get("current_state").and_then(|v| v.as_str()) {
        has_content = true;
        lines.push(format!("- Current State: {current_state}"));
    }

    if !has_content {
        return None;
    }

    Some(lines.join("\n"))
}

/// Append-only merge: list slots only ever grow (deduped), never get rewritten.
pub fn merge_memory_slots(existing: Option<&Value>, new: &Value) -> Value {
    // The summarizer's JSON-schema-constrained decode is a strong hint, not
    // a guarantee — a misbehaving/older local model can still return a
    // valid-but-wrong-shaped JSON value (a bare string, array, null...).
    // Treating that as "no new slots this pass" (falls through to an empty
    // object, so every field below just keeps whatever `existing` had) is
    // the safe degradation; unwrapping unconditionally used to panic and
    // silently kill the whole in-flight turn task.
    let empty = serde_json::Map::new();
    let new = new.as_object().unwrap_or(&empty);
    let mut merged: HashMap<String, Vec<String>> = HashMap::new();

    for &key in MEMORY_SLOT_KEYS {
        let mut items: Vec<String> = existing
            .and_then(|e| e.get(key))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let mut seen: std::collections::HashSet<String> =
            items.iter().map(|s| s.trim().to_lowercase()).collect();

        if let Some(new_items) = new.get(key).and_then(|v| v.as_array()) {
            for item in new_items {
                if let Some(s) = item.as_str() {
                    let trimmed = s.trim().to_string();
                    let lower = trimmed.to_lowercase();
                    if !trimmed.is_empty() && !seen.contains(&lower) {
                        items.push(trimmed);
                        seen.insert(lower);
                    }
                }
            }
        }

        merged.insert(key.to_string(), items);
    }

    let current_state = new
        .get("current_state")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            existing
                .and_then(|e| e.get("current_state"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
        });

    let mut obj = serde_json::Map::new();
    for &key in MEMORY_SLOT_KEYS {
        obj.insert(
            key.to_string(),
            Value::Array(
                merged
                    .get(key)
                    .map(|v| v.iter().map(|s| json!(s)).collect())
                    .unwrap_or_default(),
            ),
        );
    }
    obj.insert("current_state".to_string(), json!(current_state));

    Value::Object(obj)
}

/// Bounded, isolated shrink: when a single list grows past max_items, keep only the most recent.
pub fn consolidate_slot_if_needed(mut slots: Value, max_items: i32) -> Value {
    let max_items = max_items as usize;
    if let Some(obj) = slots.as_object_mut() {
        for key in MEMORY_SLOT_KEYS {
            if let Some(Value::Array(arr)) = obj.get_mut(*key) {
                if arr.len() > max_items {
                    *arr = arr.split_off(arr.len() - max_items);
                }
            }
        }
    }
    slots
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
    let lines: Vec<&str> = fence_block.lines().collect();
    if lines.len() < 2 {
        return fence_block.to_string();
    }
    let opening = lines[0];
    let closing = lines[lines.len() - 1];
    let body = &lines[1..lines.len() - 1];

    if body.len() <= (head_lines + tail_lines) as usize {
        return fence_block.to_string();
    }

    let elided = body.len() - (head_lines + tail_lines) as usize;
    let mut kept: Vec<String> = Vec::new();
    kept.extend(body[..head_lines as usize].iter().map(|s| s.to_string()));
    kept.push(format!("[...{elided} lines elided...]"));
    if tail_lines > 0 {
        kept.extend(
            body[body.len() - tail_lines as usize..]
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
pub fn find_reserve_floor_rowid(rows: &[Value], reserve_exchanges: i32) -> i64 {
    let exchanges = group_into_exchanges(rows);
    if exchanges.len() <= reserve_exchanges as usize {
        return rows
            .first()
            .and_then(|r| r.get("rowid").and_then(|v| v.as_i64()))
            .unwrap_or(0);
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
pub async fn run_compaction(
    pool: &SqlitePool,
    session_id: &str,
    summarizer: &SummarizerClient,
    token_cfg: &TokenManagementConfig,
    summarizer_cfg: &SummarizerConfig,
    context_length: i32,
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
        context_length,
    )
    .await;

    // Always release, whether the pass succeeded, bailed out early, or
    // failed — `update_compaction_state` (success path) already sets
    // compaction_state back to 'idle', so this is a harmless no-op there.
    let _ = sessions::release_compaction_lock(pool, session_id).await;

    result
}

async fn run_compaction_inner(
    pool: &SqlitePool,
    session_id: &str,
    summarizer: &SummarizerClient,
    token_cfg: &TokenManagementConfig,
    summarizer_cfg: &SummarizerConfig,
    context_length: i32,
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

    let total_tokens: i32 = rows.iter().map(|r| r.token_count.unwrap_or(0)).sum();
    let high_water = token_cfg
        .min_compaction_tokens
        .max((context_length as f64 * token_cfg.compaction_threshold) as i32);

    if total_tokens <= high_water {
        return None;
    }

    let low_water = (context_length as f64 * token_cfg.compaction_target_ratio) as i32;

    let candidate_exchanges = group_into_exchanges(&candidate_rows);
    let mut to_fold: Vec<Value> = Vec::new();
    let mut remaining_tokens = total_tokens;

    // Calculate per-exchange token count
    for exchange in &candidate_exchanges {
        let exchange_tokens: i32 = exchange
            .iter()
            .map(|v| {
                v.get("rowid")
                    .and_then(|r| r.as_i64())
                    .and_then(|rowid| {
                        rows.iter()
                            .find(|r| r.rowid == rowid)
                            .map(|r| r.token_count.unwrap_or(0))
                    })
                    .unwrap_or(0)
            })
            .sum();

        to_fold.extend(exchange.clone());
        remaining_tokens -= exchange_tokens;

        if remaining_tokens <= low_water {
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
    let merged = consolidate_slot_if_needed(merged, summarizer_cfg.max_slot_items);

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

    let tokens_after = total_tokens
        - to_fold
            .iter()
            .map(|v| {
                v.get("rowid")
                    .and_then(|r| r.as_i64())
                    .and_then(|rowid| {
                        rows.iter()
                            .find(|r| r.rowid == rowid)
                            .map(|r| r.token_count.unwrap_or(0))
                    })
                    .unwrap_or(0)
            })
            .sum::<i32>();

    Some(CompactionResult {
        messages_compacted: to_fold.len(),
        tokens_before: total_tokens,
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
            "new_constraints": ["Use Rust"],
            "new_decisions": [],
            "new_completions": ["Wrote main.rs"],
            "current_state": "Implementing tests"
        });
        let block = render_memory_block(Some(&slots));
        assert!(block.is_some());
        let block = block.unwrap();
        assert!(block.contains("[CONSOLIDATED PROJECT MEMORY]"));
        assert!(block.contains("Use Rust"));
        assert!(block.contains("Wrote main.rs"));
        assert!(block.contains("Implementing tests"));
    }

    #[test]
    fn test_merge_memory_slots_append_only() {
        let existing = json!({
            "new_constraints": ["Use Rust"],
            "new_decisions": ["Use async"],
            "new_completions": ["Wrote main.rs"]
        });
        let new = json!({
            "new_constraints": ["Use Rust", "No malloc"],
            "new_decisions": ["Use sync"],
            "new_completions": ["Wrote lib.rs"]
        });
        let merged = merge_memory_slots(Some(&existing), &new);
        let constraints = merged.get("new_constraints").unwrap().as_array().unwrap();
        assert_eq!(constraints.len(), 2); // "Use Rust" deduped, "No malloc" added
    }

    #[test]
    fn test_merge_memory_slots_non_object_new_does_not_panic() {
        let existing = json!({
            "new_constraints": ["Use Rust"],
            "new_decisions": [],
            "new_completions": [],
            "current_state": "steady"
        });
        // A misbehaving summarizer returning something JSON-valid but not
        // the expected object shape must not panic — it should just
        // contribute nothing this pass, leaving `existing` intact.
        for malformed in [json!("oops"), json!([1, 2, 3]), json!(null), json!(42)] {
            let merged = merge_memory_slots(Some(&existing), &malformed);
            let constraints = merged.get("new_constraints").unwrap().as_array().unwrap();
            assert_eq!(constraints.len(), 1);
            assert_eq!(constraints[0], "Use Rust");
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
            "new_constraints".to_string(),
            json!(["a", "b", "c", "d", "e"]),
        );
        slots.insert("new_decisions".to_string(), json!(["x"]));
        slots.insert("new_completions".to_string(), json!(["w"]));
        let slots = Value::Object(slots);
        let consolidated = consolidate_slot_if_needed(slots, 3);
        let constraints = consolidated
            .get("new_constraints")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(constraints.len(), 3);
        assert_eq!(constraints[0].as_str().unwrap(), "c");
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
}
