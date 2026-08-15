use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::agent::compaction::{
    apply_content_mask, apply_tool_mask, emergency_trim, find_reserve_floor_rowid,
    render_memory_block, stored_content_as_text,
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
        thought_seed: Option<&str>,
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
            // A first message with image attachments stores `content` as a
            // blocks JSON array full of base64 payloads — inlining it here
            // would put megabytes of base64 into the permanent system head
            // of EVERY turn. Collapse non-text blocks to a placeholder.
            messages.push(json!({
                "role": "system",
                "content": format!(
                    "[Original request]\n{}",
                    stored_content_as_text(
                        row.content.as_deref().unwrap_or(""),
                        row.content_format.as_deref()
                    )
                )
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

        let live_messages: Vec<Value> = live_rows.iter().map(row_to_message).collect();

        let reserve_floor = find_reserve_floor_rowid(&live_messages, self.reserve_exchanges);
        let live_messages = apply_tool_mask(&live_messages, reserve_floor, &self.config);
        let live_messages = apply_content_mask(&live_messages, reserve_floor, &self.config);
        let live_messages = self.enforce_live_tail_budget(&live_messages, reserve_floor);

        // Computed from the FINAL live messages (post-masking, post-budget),
        // not the raw rows: the emergency valve below compares this against
        // the cap, and the pre-masking sum could trip the destructive trim
        // when the tail was already under budget.
        let live_token_sum: i32 = count_messages_tokens(&live_messages);

        let head = messages.clone();
        messages.extend(live_messages);

        // Layer 7 (tail-region): behavioral-memory recall block. Placed
        // immediately before the new user message — inside the region that
        // changes every turn regardless — so the shared prefix (head + live
        // tail up to here) stays byte-identical for prompt-prefix caching.
        // Injected verbatim: the engine's `antisycophancy::render_block`
        // already emits its own `[Working assumptions about you]` /
        // `[Worth testing this turn]` / `[Where I'm unsure]` /
        // `[Check yourself]` headers and closing footer. This used to wrap
        // it in a second `[Adaptive Pathway hints]` header, which both named
        // a retired subsystem and gave the block two nested labels.
        // Tokens of the injected tail blocks are counted so the emergency
        // valve below doesn't undercount the real prompt by their size.
        let mut tail_extra_tokens: i32 = 0;
        if let Some(hints) = ap_hints {
            if !hints.trim().is_empty() {
                let block = json!({
                    "role": "system",
                    "content": hints
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

        // Layer 8 (tail-region): thought-seeded episodic recall, appended
        // AFTER the new user message as a trailing partial `assistant` turn
        // -- a structurally different insertion point from `ap_hints`/
        // `retrieved` above (which land BEFORE the user message as
        // system-role blocks). Only ever populated when the caller has
        // already confirmed the active provider/model combination honors a
        // trailing assistant-role prefill (`Provider::supports_assistant_prefill`
        // + `reasoning_models::supports_reasoning` -- see
        // `AgentLoop::pathway_recall`); this function doesn't know or care
        // why, it just appends what it's given. `None`/empty produces zero
        // delta, same contract as `ap_hints`/`retrieved`. Mutually exclusive
        // with the `retrieved` system block in practice (the caller picks
        // one path per turn), but nothing here enforces that -- it's a
        // caller-side policy, not a structural one.
        let mut seeded = false;
        if let Some(seed) = thought_seed {
            if !seed.trim().is_empty() {
                messages.push(json!({
                    "role": "assistant",
                    "content": format!("<think>\n{seed}\n")
                }));
                seeded = true;
            }
        }

        // The tail region the emergency-trim rebuild below must preserve
        // verbatim: the new user message, plus the thought-seed turn if one
        // was appended.
        let tail_start = if seeded { 2 } else { 1 };
        let tail_messages: Vec<Value> = messages[messages.len() - tail_start..].to_vec();

        // Emergency valve check
        let max_context_tokens =
            max_context_tokens_override.unwrap_or(self.config.max_context_tokens);
        let emergency_cap = (max_context_tokens as f64 * EMERGENCY_TRIM_RATIO) as i32;

        let system_tokens = count_messages_tokens(&head);
        let new_msg_tokens = count_messages_tokens(&tail_messages);
        let total_tokens =
            live_token_sum + system_tokens + new_msg_tokens + tail_extra_tokens;

        if total_tokens > emergency_cap {
            let target = (max_context_tokens as f64 * self.config.compaction_target_ratio) as i32;
            let live_messages = &messages[head.len()..messages.len() - tail_start];
            let trimmed_live = emergency_trim(live_messages, reserve_floor, target);
            messages = [head, trimmed_live, tail_messages].concat();
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

/// Strip a trailing thought-seed prefill off `messages`, returning it.
///
/// `build_messages` appends the `thought_seed` as a trailing `assistant`
/// message (an ephemeral `<think>` prefill for the provider's eyes only —
/// see Layer 8 above). It is NOT transcript content: persisted, the literal
/// `<think>` seed lands in saved chats, and the next turn's request would
/// carry two adjacent assistant messages (a 400 on Anthropic). The caller
/// (`agent::loop_::run_inner`) strips it before the first `save_messages`
/// and hands it to the tool loop separately, which appends it to the
/// outgoing provider request only.
///
/// Matches on the exact shape `build_messages` produces — a trailing
/// `assistant` message with no DB `id` whose content is a `<think>` block —
/// so a genuinely persisted trailing assistant message is never touched.
pub fn strip_trailing_thought_seed(messages: &mut Vec<Value>) -> Option<Value> {
    let is_seed = messages.last().is_some_and(|m| {
        m.get("role").and_then(|r| r.as_str()) == Some("assistant")
            && m.get("id").is_none()
            && m
                .get("content")
                .and_then(|c| c.as_str())
                .is_some_and(|c| c.starts_with("<think>\n"))
    });
    if is_seed {
        messages.pop()
    } else {
        None
    }
}

/// Convert a DB row to a message Value.
fn row_to_message(row: &crate::storage::messages::MessageRow) -> Value {    let mut msg = serde_json::Map::new();
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

    /// Companion to the byte-identity test above, for `thought_seed`: `None`
    /// must be byte-identical to the no-seed baseline (zero prompt delta,
    /// same contract as `ap_hints`/`retrieved`), and `Some` must change only
    /// the trailing region — every message up to and including the new user
    /// message stays byte-identical, with exactly one trailing `assistant`
    /// message appended after it.
    #[tokio::test]
    async fn build_messages_thought_seed_only_changes_the_trailing_region() {
        let pool = test_pool().await;
        sessions::create_session(&pool, "sess-1", "Test")
            .await
            .unwrap();
        let builder = ContextBuilder::new(pool.clone(), TokenManagementConfig::default(), 2);
        let mut seed = vec![json!({"role": "user", "content": "hello"})];
        builder.save_messages("sess-1", &mut seed).await.unwrap();

        let baseline = builder
            .build_messages(
                "sess-1", "next", None, None, None, Some("C:\\c"), Some("C:\\w"), None, None, None,
            )
            .await
            .unwrap();
        let with_empty_seed = builder
            .build_messages(
                "sess-1", "next", None, None, None, Some("C:\\c"), Some("C:\\w"), None, None, Some("   "),
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_string(&baseline).unwrap(),
            serde_json::to_string(&with_empty_seed).unwrap(),
            "empty/None thought_seed must not change the prompt"
        );

        let with_seed = builder
            .build_messages(
                "sess-1",
                "next",
                None,
                None,
                None,
                Some("C:\\c"),
                Some("C:\\w"),
                None,
                None,
                Some("The user prefers terse answers."),
            )
            .await
            .unwrap();

        assert_eq!(with_seed.len(), baseline.len() + 1, "seeding must add exactly one trailing message");
        // Every message up to and including the user message is untouched.
        assert_eq!(
            serde_json::to_string(&with_seed[..baseline.len()]).unwrap(),
            serde_json::to_string(&baseline).unwrap(),
            "thought_seed must never alter anything before the trailing seed message"
        );
        let last = with_seed.last().unwrap();
        assert_eq!(last.get("role").and_then(|r| r.as_str()), Some("assistant"));
        assert!(
            last.get("content")
                .and_then(|c| c.as_str())
                .map(|c| c.contains("The user prefers terse answers.") && c.starts_with("<think>"))
                .unwrap_or(false)
        );
        // The user message must still be the second-to-last, unchanged.
        let user_msg = &with_seed[with_seed.len() - 2];
        assert_eq!(user_msg.get("role").and_then(|r| r.as_str()), Some("user"));
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
            .build_messages("sess-1", "hello", None, None, None, None, None, None, None, None)
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
            .build_messages("sess-1", "next", None, None, None, Some("C:\\c"), Some("C:\\w"), None, None, None)
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
                        .map(|c| c.contains("Use write instead of edit for new files."))
                        .unwrap_or(false)
            })
            .count();
        assert_eq!(hints_system, 1, "exactly one AP hint system message");
        // Injected verbatim. The engine's `antisycophancy::render_block`
        // already emits its own section headers and footer; this used to add
        // a second `[Adaptive Pathway hints]` wrapper on top, naming a
        // retired subsystem and giving the block two nested labels.
        assert!(
            !with_hints.iter().any(|m| m
                .get("content")
                .and_then(|c| c.as_str())
                .map(|c| c.contains("Adaptive Pathway hints"))
                .unwrap_or(false)),
            "the recall block must be injected verbatim, not re-wrapped"
        );
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
                .map(|c| c.contains("Use write instead of edit for new files."))
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

    /// Regression: when the first user message has images, its stored
    /// `content` is a blocks JSON array full of base64 payloads — and that
    /// raw JSON used to be inlined into the `[Original request]` anchor,
    /// i.e. into the permanent system head of every turn. Non-text blocks
    /// must collapse to a `[N image(s) attached]` placeholder.
    #[tokio::test]
    async fn build_messages_anchor_collapses_image_blocks_to_a_placeholder() {
        let pool = test_pool().await;
        sessions::create_session(&pool, "sess-1", "Test")
            .await
            .unwrap();
        let builder = ContextBuilder::new(pool.clone(), TokenManagementConfig::default(), 2);

        let base64_payload = "QUJD".repeat(1000); // ~4KB of stand-in base64
        let mut seed = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "what is in this picture?"},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{base64_payload}")}},
            ]
        })];
        builder.save_messages("sess-1", &mut seed).await.unwrap();

        let messages = builder
            .build_messages("sess-1", "next", None, None, None, None, None, None, None, None)
            .await
            .unwrap();

        let anchor = messages
            .iter()
            .find(|m| {
                m.get("content")
                    .and_then(|c| c.as_str())
                    .map(|c| c.contains("[Original request]"))
                    .unwrap_or(false)
            })
            .expect("the anchor system message must exist");
        let anchor_text = anchor["content"].as_str().unwrap();
        assert!(
            anchor_text.contains("what is in this picture?"),
            "the text block survives: {anchor_text:?}"
        );
        assert!(
            anchor_text.contains("[1 image(s) attached]"),
            "the image collapses to a placeholder: {anchor_text:?}"
        );
        assert!(
            !anchor_text.contains(&base64_payload[..64]),
            "no base64 in the permanent system head"
        );
    }

    /// Regression: the thought-seed prefill (a trailing `<think>` assistant
    /// message with no id) used to be persisted into the transcript by
    /// `save_messages`. `strip_trailing_thought_seed` must remove exactly
    /// that message — and only that message — before persistence.
    #[tokio::test]
    async fn thought_seed_is_stripped_before_persistence() {
        let pool = test_pool().await;
        sessions::create_session(&pool, "sess-1", "Test")
            .await
            .unwrap();
        let builder = ContextBuilder::new(pool.clone(), TokenManagementConfig::default(), 2);
        let mut seed = vec![json!({"role": "user", "content": "hello"})];
        builder.save_messages("sess-1", &mut seed).await.unwrap();

        let mut messages = builder
            .build_messages(
                "sess-1", "next", None, None, None, None, None, None, None,
                Some("The user prefers terse answers."),
            )
            .await
            .unwrap();

        let stripped = strip_trailing_thought_seed(&mut messages)
            .expect("the seed message must be stripped");
        assert_eq!(stripped["role"], "assistant");
        assert!(
            stripped["content"]
                .as_str()
                .unwrap()
                .contains("The user prefers terse answers.")
        );
        // The user message is now last again.
        assert_eq!(
            messages.last().unwrap().get("role").and_then(|r| r.as_str()),
            Some("user")
        );

        // Persisting the stripped vec must not write any `<think>` content.
        builder.save_messages("sess-1", &mut messages).await.unwrap();
        let rows = messages::get_messages_by_session(&pool, "sess-1")
            .await
            .unwrap();
        assert!(
            rows.iter().all(|r| {
                r.content
                    .as_deref()
                    .map(|c| !c.contains("<think>"))
                    .unwrap_or(true)
            }),
            "no persisted row may contain the thought seed: {rows:?}"
        );
    }

    /// A trailing assistant message that is NOT a thought seed (a real,
    /// already-persisted reply — it has an `id`) must never be stripped.
    #[test]
    fn strip_trailing_thought_seed_leaves_real_assistant_messages() {
        let mut messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"id": "m-1", "role": "assistant", "content": "<think>\nreal reply"}),
        ];
        assert!(strip_trailing_thought_seed(&mut messages).is_none());
        assert_eq!(messages.len(), 2);
    }
}
