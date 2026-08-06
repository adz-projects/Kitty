use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::agent::compaction::{
    apply_content_mask, apply_tool_mask, emergency_trim, find_reserve_floor_rowid,
    render_memory_block,
};
use crate::agent::tokens::count_messages_tokens;
use crate::config::TokenManagementConfig;

use crate::storage::messages;
use crate::storage::sessions;

pub const BASE_PERSONA: &str =
    "You are a helpful, precise AI assistant. Respond concisely and accurately.";

const EMERGENCY_TRIM_RATIO: f64 = 0.9;

/// Assembles the full message context for one LLM turn.
pub struct ContextBuilder {
    pool: SqlitePool,
    pub config: TokenManagementConfig,
    reserve_exchanges: i32,
}

impl ContextBuilder {
    pub fn new(pool: SqlitePool, config: TokenManagementConfig, reserve_exchanges: i32) -> Self {
        Self {
            pool,
            config,
            reserve_exchanges,
        }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn config(&self) -> &TokenManagementConfig {
        &self.config
    }

    /// Build the full context for an LLM call.
    ///
    /// `ap_hints` is a **per-turn** Adaptive-Pathway hint block (returned by the
    /// `decide` tool at turn start). It is deliberately injected into the
    /// live-tail/new-message region — right before the new user message that
    /// changes every turn anyway — NOT into the stable head (layers 1-5) or the
    /// sorted tool-hints layer. Inserting it into the head would change the
    /// shared prompt prefix turn-over-turn and defeat llama-server/OpenAI KV
    /// prefix caching every turn (the head must stay byte-identical; see
    /// `build_messages_is_byte_identical_across_repeat_calls_with_identical_input`).
    /// `None` (or an empty block) produces **zero** delta to the prompt, so a
    /// disabled/unreachable AP — or a turn where it returned nothing — leaves
    /// the cached prefix untouched.
    #[allow(clippy::too_many_arguments)]
    pub async fn build_messages(
        &self,
        session_id: &str,
        new_message: &str,
        persona_override: Option<&str>,
        images: Option<&[serde_json::Value]>,
        max_context_tokens_override: Option<i32>,
        chat_dir: Option<&str>,
        cwd: Option<&str>,
        ap_hints: Option<&str>,
        retrieved: Option<&str>,
    ) -> Result<Vec<Value>, String> {
        let session = sessions::get_session(&self.pool, session_id)
            .await
            .map_err(|e| format!("Failed to fetch session: {}", e))?
            .ok_or_else(|| format!("Session {} not found", session_id))?;

        let compacted_through = session.compacted_through_rowid;
        let memory_slots: Option<Value> = session
            .memory_slots
            .as_ref()
            .and_then(|s| serde_json::from_str(s).ok());

        let mut messages: Vec<Value> = Vec::new();

        // Layer 1: Base persona
        messages.push(json!({
            "role": "system",
            "content": BASE_PERSONA
        }));

        // Layer 2: Session override
        if let Some(persona) = persona_override {
            messages.push(json!({
                "role": "system",
                "content": persona
            }));
        }

        // Layer 2.5: Writable directory hint
        let mut writable_dirs = Vec::new();
        if let Some(d) = chat_dir {
            writable_dirs.push(d);
        }
        if let Some(d) = cwd {
            if Some(d) != chat_dir && !writable_dirs.contains(&d) {
                writable_dirs.push(d);
            }
        }
        if !writable_dirs.is_empty() {
            let where_str = if writable_dirs.len() == 1 {
                writable_dirs[0].to_string()
            } else {
                writable_dirs.join(" or ")
            };
            messages.push(json!({
                "role": "system",
                "content": format!(
                    "Your file tools (read/write/edit/list) are scoped to {}. \
                     Any files the user attached to this chat live there too — use that \
                     path directly instead of searching for one.",
                    where_str
                )
            }));
        }

        // Layer 4: Anchor the first user message
        //
        // (Layer 3 — a prose restatement of every active tool's name and
        // description, "You have access to the following MCP tools: …" —
        // was removed. The real `tools` array sent alongside these messages
        // already carries that information in the structured form the chat
        // template renders; duplicating it here meant a model read every
        // tool description twice, once correctly and once as truncated
        // imperative prose. Observed cause of a Qwen3.6/llama-server session
        // reciting tool docstrings back verbatim instead of answering: with
        // ~40 registered tools this block ran to dozens of lines of
        // "MANDATORY", "You MUST call this" (adaptive-pathway's docstrings,
        // since trimmed — see `plugins/adaptive-pathway`), each truncated
        // mid-word at 120 characters.)
        let first_user_row = messages::get_first_user_message(&self.pool, session_id)
            .await
            .map_err(|e| format!("Failed to fetch first message: {}", e))?;

        let _anchor_first_user = first_user_row.is_some();
        if let Some(row) = &first_user_row {
            messages.push(json!({
                "role": "system",
                "content": format!("[Original request]\n{}", row.content.as_deref().unwrap_or(""))
            }));
        }

        // Layer 5: Consolidated memory from prior compaction
        if let Some(block) = render_memory_block(memory_slots.as_ref()) {
            messages.push(json!({
                "role": "system",
                "content": block
            }));
        }

        // Layer 6: Live tail (messages after compacted_through), Tier-1 masked
        let mut live_rows =
            messages::get_messages_after_rowid(&self.pool, session_id, compacted_through)
                .await
                .map_err(|e| format!("Failed to fetch live messages: {}", e))?;

        // Remove system messages
        live_rows.retain(|r| r.role != "system");

        // Remove anchored first user message
        if let Some(fur) = &first_user_row {
            live_rows.retain(|r| r.rowid != fur.rowid);
        }

        let live_token_sum: i32 = live_rows.iter().map(|r| r.token_count.unwrap_or(0)).sum();

        let live_messages: Vec<Value> = live_rows.iter().map(row_to_message).collect();

        let reserve_floor = find_reserve_floor_rowid(&live_messages, self.reserve_exchanges);
        let live_messages = apply_tool_mask(&live_messages, reserve_floor, &self.config);
        let live_messages = apply_content_mask(&live_messages, reserve_floor, &self.config);
        let live_messages = self.enforce_live_tail_budget(&live_messages, reserve_floor);

        let head = messages.clone();
        messages.extend(live_messages);

        // Layer 7 (tail-region): Adaptive Pathway per-turn hints. Placed
        // immediately before the new user message — inside the region that
        // changes every turn regardless — so the shared prefix (head + live
        // tail up to here) stays byte-identical for prompt-prefix caching.
        // The sidecar's decide payload carries a `hints` list already
        // rendered as plain text here; see `AgentLoop::run`'s AP wiring.
        // Tokens of the injected tail blocks are counted so the emergency
        // valve below doesn't undercount the real prompt by their size.
        let mut tail_extra_tokens: i32 = 0;
        if let Some(hints) = ap_hints {
            if !hints.trim().is_empty() {
                let block = json!({
                    "role": "system",
                    "content": format!("[Adaptive Pathway hints]\n{hints}")
                });
                tail_extra_tokens += count_messages_tokens(std::slice::from_ref(&block));
                messages.push(block);
            }
        }

        // Layer 7.5 (tail-region): pre-flight memory recall. Injected in the
        // tail — immediately before the new user message, like AP hints — so
        // the stable head + live tail stay byte-identical for prompt-prefix
        // caching. `None` (preflight disabled, no recall intent, or no match)
        // produces zero delta. The block is a `role: "system"` message, which
        // `save_messages` never persists and the transcript never renders.
        if let Some(retrieved) = retrieved {
            if !retrieved.trim().is_empty() {
                let block = json!({
                    "role": "system",
                    "content": retrieved
                });
                tail_extra_tokens += count_messages_tokens(std::slice::from_ref(&block));
                messages.push(block);
            }
        }

        // Append new user message
        if let Some(imgs) = images {
            let mut blocks: Vec<Value> = vec![json!({
                "type": "text",
                "text": new_message
            })];
            blocks.extend(imgs.to_vec());
            messages.push(json!({
                "role": "user",
                "content": blocks
            }));
        } else {
            messages.push(json!({
                "role": "user",
                "content": new_message
            }));
        }

        let tail_new_message = messages.last().cloned().unwrap_or_default();

        // Emergency valve check
        let max_context_tokens =
            max_context_tokens_override.unwrap_or(self.config.max_context_tokens);
        let emergency_cap = (max_context_tokens as f64 * EMERGENCY_TRIM_RATIO) as i32;

        let system_tokens = count_messages_tokens(&head);
        let new_msg_tokens = count_messages_tokens(std::slice::from_ref(&tail_new_message));
        let total_tokens =
            live_token_sum + system_tokens + new_msg_tokens + tail_extra_tokens;

        if total_tokens > emergency_cap {
            let target = (max_context_tokens as f64 * self.config.compaction_target_ratio) as i32;
            let live_messages = &messages[head.len()..messages.len() - 1];
            let trimmed_live = emergency_trim(live_messages, reserve_floor, target);
            messages = [head, trimmed_live, vec![tail_new_message]].concat();
        }

        Ok(messages)
    }

    /// Count tokens for a set of messages.
    pub fn count_tokens(&self, messages: &[Value]) -> i32 {
        count_messages_tokens(messages)
    }

    /// Persist new messages (those without a DB `id`) to the database.
    ///
    /// Takes `&mut` and writes the generated `id` back onto each message it
    /// persists. Without this, the in-memory turn's `messages` vec never
    /// gained an `id`, so every subsequent call in the same turn (this is
    /// called after nearly every step) saw the *same* already-saved
    /// messages as new again, re-inserting them under fresh UUIDs each
    /// time — quadratic duplicate growth per turn.
    pub async fn save_messages(
        &self,
        session_id: &str,
        message_dicts: &mut [Value],
    ) -> Result<(), String> {
        use crate::storage::messages::MessageRow;

        let mut rows = Vec::new();
        let mut ids: Vec<(usize, String)> = Vec::new();
        for (idx, msg) in message_dicts.iter().enumerate() {
            // Skip system messages and already-persisted messages
            if msg.get("role").and_then(|r| r.as_str()) == Some("system") {
                continue;
            }
            if msg.get("id").is_some() {
                continue;
            }

            let id = uuid::Uuid::new_v4().to_string();
            ids.push((idx, id.clone()));
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");

            let (content, content_format) = if let Some(c) = msg.get("content") {
                if c.is_array() {
                    (
                        Some(serde_json::to_string(c).unwrap_or_default()),
                        Some("blocks".to_string()),
                    )
                } else if let Some(s) = c.as_str() {
                    (Some(s.to_string()), Some("text".to_string()))
                } else {
                    (None, Some("text".to_string()))
                }
            } else {
                (None, Some("text".to_string()))
            };

            let tool_calls = msg
                .get("tool_calls")
                .and_then(|tc| serde_json::to_string(tc).ok());

            let tool_call_id = msg
                .get("tool_call_id")
                .and_then(|tcid| tcid.as_str())
                .map(|s| s.to_string());

            let token_count = count_messages_tokens(std::slice::from_ref(msg));

            rows.push(MessageRow {
                rowid: 0,
                id,
                session_id: session_id.to_string(),
                role: role.to_string(),
                content,
                tool_calls,
                tool_call_id,
                token_count: Some(token_count),
                content_format,
                created_at: None,
            });
        }

        if !rows.is_empty() {
            messages::save_messages(&self.pool, session_id, &rows)
                .await
                .map_err(|e| format!("Failed to save messages: {}", e))?;
        }

        for (idx, id) in ids {
            if let Some(obj) = message_dicts[idx].as_object_mut() {
                obj.insert("id".to_string(), Value::String(id));
            }
        }

        // Update session timestamp
        sessions::update_session(&self.pool, session_id)
            .await
            .map_err(|e| format!("Failed to update session: {}", e))?;

        Ok(())
    }

    /// Per-turn budget check for the live tail.
    fn enforce_live_tail_budget(&self, live_messages: &[Value], reserve_floor: i64) -> Vec<Value> {
        let budget = self.config.max_live_tail_tokens;
        if self.count_tokens(live_messages) <= budget {
            return live_messages.to_vec();
        }

        // Phase A: collapse tool messages to bare markers
        let zero_mask_cfg = TokenManagementConfig {
            tool_mask_head: 0,
            tool_mask_tail: 0,
            ..self.config.clone()
        };
        let masked = apply_tool_mask(live_messages, reserve_floor, &zero_mask_cfg);
        if self.count_tokens(&masked) <= budget {
            return masked;
        }

        // Phase B: drop whole eligible exchanges
        emergency_trim(live_messages, reserve_floor, budget)
    }
}

/// Convert a DB row to a message Value.
fn row_to_message(row: &crate::storage::messages::MessageRow) -> Value {
    let mut msg = serde_json::Map::new();
    msg.insert("id".to_string(), json!(row.id));
    msg.insert("rowid".to_string(), json!(row.rowid));
    msg.insert("role".to_string(), json!(row.role));

    if let Some(ref content) = row.content {
        if row.content_format.as_deref() == Some("blocks") {
            if let Ok(parsed) = serde_json::from_str(content) {
                msg.insert("content".to_string(), parsed);
            } else {
                msg.insert("content".to_string(), json!(content));
            }
        } else {
            msg.insert("content".to_string(), json!(content));
        }
    }

    if let Some(ref tc) = row.tool_calls {
        if let Ok(parsed) = serde_json::from_str(tc) {
            msg.insert("tool_calls".to_string(), parsed);
        }
    }
    if let Some(ref tcid) = row.tool_call_id {
        msg.insert("tool_call_id".to_string(), json!(tcid));
    }

    Value::Object(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[test]
    fn test_base_persona() {
        assert!(!BASE_PERSONA.is_empty());
    }

    /// Regression for the bug where `save_messages` never wrote the
    /// generated `id` back onto the in-memory `Value`s it persisted: every
    /// repeat call in the same turn (this fires after nearly every step)
    /// saw the same already-saved messages as new again and re-inserted
    /// them under fresh UUIDs, growing the table ~quadratically per turn.
    #[tokio::test]
    async fn save_messages_is_idempotent_across_repeat_calls_with_the_same_vec() {
        let pool = test_pool().await;
        sessions::create_session(&pool, "sess-1", "Test")
            .await
            .unwrap();
        let builder = ContextBuilder::new(pool.clone(), TokenManagementConfig::default(), 2);

        let mut messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi there"}),
        ];

        builder
            .save_messages("sess-1", &mut messages)
            .await
            .unwrap();
        // Same call again with the SAME vec, as loop_.rs does after every
        // subsequent step in a turn — messages already saved must be
        // recognized as such via the `id` this call should have written back.
        builder
            .save_messages("sess-1", &mut messages)
            .await
            .unwrap();

        let rows = messages::get_messages_by_session(&pool, "sess-1")
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            2,
            "messages must not be duplicated across repeat save_messages calls"
        );

        // And every message in the vec should now carry the id that was
        // written back, not still be missing one.
        for msg in &messages {
            assert!(msg.get("id").and_then(|v| v.as_str()).is_some());
        }
    }

    /// Prompt-determinism regression: two `build_messages` calls with
    /// identical inputs must produce byte-identical output, or llama-server's
    /// KV prefix cache misses on every turn instead of hitting the shared
    /// prefix. Covers head layers 1-5 plus live-tail ordering.
    #[tokio::test]
    async fn build_messages_is_byte_identical_across_repeat_calls_with_identical_input() {
        let pool = test_pool().await;
        sessions::create_session(&pool, "sess-1", "Test")
            .await
            .unwrap();
        let builder = ContextBuilder::new(pool.clone(), TokenManagementConfig::default(), 2);

        let mut seed = vec![json!({"role": "user", "content": "hello"})];
        builder.save_messages("sess-1", &mut seed).await.unwrap();

        let first = builder
            .build_messages(
                "sess-1",
                "next message",
                None,
                None,
                None,
                Some("C:\\chat"),
                Some("C:\\cwd"),
                None,
                None,
            )
            .await
            .unwrap();
        let second = builder
            .build_messages(
                "sess-1",
                "next message",
                None,
                None,
                None,
                Some("C:\\chat"),
                Some("C:\\cwd"),
                None,
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap(),
            "build_messages must be byte-identical across calls with identical input"
        );
    }

    /// Guard against reviving the removed "Layer 3" tool-hints system
    /// message. Tools are sent to the provider via the real `tools` array
    /// (`agent::loop_::tools_to_openai_format`), which the chat template
    /// already renders — restating each tool's name/description a second
    /// time as system-prompt prose is what a Qwen3.6/llama-server session
    /// was observed reciting back verbatim instead of answering the user.
    #[tokio::test]
    async fn build_messages_never_restates_tool_descriptions_in_a_system_message() {
        let pool = test_pool().await;
        sessions::create_session(&pool, "sess-1", "Test")
            .await
            .unwrap();
        let builder = ContextBuilder::new(pool.clone(), TokenManagementConfig::default(), 2);

        let messages = builder
            .build_messages("sess-1", "hello", None, None, None, None, None, None, None)
            .await
            .unwrap();

        for m in &messages {
            if m.get("role").and_then(|r| r.as_str()) != Some("system") {
                continue;
            }
            let content = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
            assert!(
                !content.contains("MCP tools") && !content.contains("Use these tools"),
                "system message resurrected the removed tool-hints layer: {content:?}"
            );
        }
    }

        /// Adaptive-Pathway hints are injected into the tail region (immediately
    /// before the new user message) so the stable head stays byte-identical
    /// for prefix caching. Asserting: (1) `None` and an empty/whitespace
    /// block both produce hints-free output (zero prompt delta), and (2) a
    /// non-empty block appears as a system message adjacent to the final
    /// (user) message, not mixed into the head layers.
    #[tokio::test]
    async fn build_messages_injects_ap_hints_in_tail_not_head() {
        let pool = test_pool().await;
        sessions::create_session(&pool, "sess-1", "Test")
            .await
            .unwrap();
        let builder = ContextBuilder::new(pool.clone(), TokenManagementConfig::default(), 2);
        let mut seed = vec![json!({"role": "user", "content": "hello"})];
        builder.save_messages("sess-1", &mut seed).await.unwrap();

        let none = builder
            .build_messages("sess-1", "next", None, None, None, Some("C:\\c"), Some("C:\\w"), None, None)
            .await
            .unwrap();
        let empty = builder
            .build_messages(
                "sess-1",
                "next",
                None,
                None,
                None,
                Some("C:\\c"),
                Some("C:\\w"),
                Some("   "),
                None,
            )
            .await
            .unwrap();
        // Zero prompt delta when there's no usable hint block.
        assert_eq!(
            serde_json::to_string(&none).unwrap(),
            serde_json::to_string(&empty).unwrap(),
            "empty/None AP hints must not change the prompt"
        );

        let with_hints = builder
            .build_messages(
                "sess-1",
                "next",
                None,
                None,
                None,
                Some("C:\\c"),
                Some("C:\\w"),
                Some("Use write instead of edit for new files."),
                None,
            )
            .await
            .unwrap();

        let system_count = with_hints
            .iter()
            .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"))
            .count();
        let hints_system = with_hints
            .iter()
            .filter(|m| {
                m.get("role").and_then(|r| r.as_str()) == Some("system")
                    && m.get("content")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains("Adaptive Pathway hints"))
                        .unwrap_or(false)
            })
            .count();
        assert_eq!(hints_system, 1, "exactly one AP hint system message");
        // The hint message must sit immediately before the last (user) message.
        let last = with_hints.last().unwrap();
        assert_eq!(
            last.get("role").and_then(|r| r.as_str()),
            Some("user"),
            "new user message must stay last"
        );
        let second_last = &with_hints[with_hints.len() - 2];
        assert!(
            second_last
                .get("content")
                .and_then(|c| c.as_str())
                .map(|c| c.contains("Adaptive Pathway hints"))
                .unwrap_or(false),
            "hints must be in the tail (second-to-last), not the head"
        );
        // With hints, the head (non-hint system messages + live tail) must equal
        // the hints-free build minus exactly the injected block — assert the
        // count differs by only the one hint message.
        assert_eq!(
            with_hints.len(),
            none.len() + 1,
            "hint injection must add exactly one message"
        );
        let _ = system_count;
    }

    #[test]
    fn test_row_to_message_basic() {        let row = crate::storage::messages::MessageRow {
            rowid: 1,
            id: "msg-1".into(),
            session_id: "sess-1".into(),
            role: "user".into(),
            content: Some("Hello".into()),
            tool_calls: None,
            tool_call_id: None,
            token_count: Some(5),
            content_format: Some("text".into()),
            created_at: None,
        };
        let msg = row_to_message(&row);
        assert_eq!(msg.get("role").and_then(|r| r.as_str()), Some("user"));
        assert_eq!(msg.get("content").and_then(|c| c.as_str()), Some("Hello"));
    }
}
