//! Session CRUD over BigTiny's REST API, translated into the exact shapes the
//! frontend already consumes from the goosed path: `SessionInfo` returns,
//! goosed-style raw session objects for `list_sessions` (`parseSession` in
//! types.ts reads `sessionId`/`title`/`cwd`/`updatedAt`), and `chat://*`
//! replay events during `load`.

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::bigtiny::client::{ensure_client, BigTinyClient};
use crate::commands::{ModeInfo, SessionInfo};

/// BigTiny has no ACP-style modes handshake — HITL policy lives server-side
/// and the chat/agentic override is client-side either way. The one live field
/// is `thinking_effort`, derived from the active provider's capability so the
/// dropdown appears only where the provider actually accepts an effort control
/// (see `bigtiny::effort`); everything else is fixed.
fn session_info(app: &AppHandle, session_id: String, cwd: String) -> SessionInfo {
    let thinking_effort = crate::bigtiny::effort::thinking_effort_for(app, &session_id);
    let is_default_folder = crate::commands::is_default_folder(app, &cwd);
    SessionInfo {
        session_id,
        cwd,
        current_mode: "approve".to_string(),
        available_modes: Vec::<ModeInfo>::new(),
        thinking_effort,
        is_default_folder,
    }
}

/// Create a session (`POST /api/chat/` with the per-chat folder as `cwd`).
/// `mode` ("chat"|"agentic", matching Kitty's own `modeOverride` vocabulary
/// verbatim) seeds BigTiny's directory-sandboxing scope for this session
/// from the very first tool call, rather than leaving it unset until a
/// later `PATCH /config` — see `update_mode`'s doc comment for why that gap
/// would matter. `provider`/`model` (optional) pin this session to a specific
/// provider from birth — per-session provider isolation: the daemon stores
/// them in session metadata and resolves at send time, so later global
/// provider changes never flip this session.
#[allow(clippy::too_many_arguments)]
pub async fn create(
    app: &AppHandle,
    cwd: String,
    provider: Option<String>,
    model: Option<String>,
) -> Result<SessionInfo, String> {
    let client = ensure_client(app)?;
    // Always "agentic". The daemon's only use of `mode` is whether to add the
    // session's `cwd` to the sandbox's allowed set, and Kitty no longer has a
    // chat/agentic distinction to withhold it for — a session's own working
    // directory is the one place it should always be able to reach.
    let mut body = json!({ "cwd": cwd, "mode": "agentic" });
    if provider.is_some() {
        body["provider"] = json!(provider);
    }
    if model.is_some() {
        body["model"] = json!(model);
    }
    let result = client.post_json("/api/chat/", &body).await?;
    let session_id = result
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or("BigTiny did not return a session id")?
        .to_string();
    let _ = app.emit("session://created", json!({ "sessionId": session_id }));
    Ok(session_info(app, session_id, cwd))
}


/// Repoint a session's *current* working directory ("Set as working
/// directory", agentic mode only) — mutates the session in place rather
/// than forking a new one, since BigTiny's directory sandbox needs to see
/// this session's `cwd` diverge from its original `chat_dir`, not disappear
/// into a brand-new session that never had one.
pub async fn update_cwd(app: &AppHandle, session_id: &str, cwd: &str) -> Result<(), String> {
    let client = ensure_client(app)?;
    client
        .patch_json(
            &format!("/api/chat/{session_id}/config"),
            &json!({ "cwd": cwd }),
        )
        .await?;
    Ok(())
}

/// Repoint a session back to a private per-chat folder ("return to thought
/// partner") and return the refreshed `SessionInfo` (so the caller sees the
/// new cwd and `is_default_folder: true`). `cwd` is a fresh per-chat folder
/// from `resolve_cwd`; this only PATCHes the session and rebuilds the info.
pub async fn reset_cwd(
    app: &AppHandle,
    session_id: &str,
    cwd: String,
) -> Result<SessionInfo, String> {
    update_cwd(app, session_id, &cwd).await?;
    Ok(session_info(app, session_id.to_string(), cwd))
}

/// Set a session's custom/default persona (`PATCH /api/chat/{id}/config`).
/// BigTiny's `ContextBuilder::build_messages` reads this from session
/// metadata and renders it as a real `role: "system"` message — the same
/// mechanism `RecipeEngine::execute` uses for a recipe's instructions. This
/// replaces the old client-side hack (chatStore's `send()` used to prepend a
/// literal `<system>...</system>` block onto the first outgoing *user*
/// message's text, a leftover from the pre-BigTiny Goose/ACP backend, which
/// had no system-prompt field of its own) — embedding fake role markup
/// inside a user turn is exactly the kind of malformed input a model whose
/// chat template expects strict role/tag structure (e.g. Qwen's tool-calling
/// template) can derail on.
pub async fn update_persona_override(
    app: &AppHandle,
    session_id: &str,
    persona: &str,
) -> Result<(), String> {
    let client = ensure_client(app)?;
    client
        .patch_json(
            &format!("/api/chat/{session_id}/config"),
            &json!({ "persona_override": persona }),
        )
        .await?;
    Ok(())
}

/// Hand a session's reasoning-effort choice to the daemon
/// (`PATCH /api/chat/{id}/config`, shallow-merged into metadata). The agent
/// loop reads `thinking_effort` beside `sampling_preset` and translates it per
/// provider dialect at send time — exactly the same transport `sampling_preset`
/// already uses, so no new route is involved.
pub async fn update_thinking_effort(
    app: &AppHandle,
    session_id: &str,
    value: &str,
) -> Result<(), String> {
    let client = ensure_client(app)?;
    client
        .patch_json(
            &format!("/api/chat/{session_id}/config"),
            &json!({ "thinking_effort": value }),
        )
        .await?;
    Ok(())
}

/// Manual context compaction (`/compact`): `POST /api/chat/{id}/compact`
/// forces the daemon to fold the session's oldest un-compacted exchanges
/// into memory, bypassing the automatic token threshold. Returns the
/// daemon's `{compacted, messages_compacted, tokens_before, tokens_after}`
/// so the UI can report what actually happened (or `{compacted: false}` when
/// there was nothing to fold).
///
/// Goes through the long-timeout request variant: the daemon answers this
/// POST only *after* running a full LLM summarization — minutes on a cold
/// local-model load, which used to blow straight through the 10s default
/// (the client gave up while the daemon kept working).
pub async fn compact(app: &AppHandle, session_id: &str) -> Result<Value, String> {
    let client = ensure_client(app)?;
    client
        .post_json_long(&format!("/api/chat/{session_id}/compact"), &json!({}))
        .await
}

/// Fetch `/api/chat/{id}/stats` — includes `compacted_through_rowid` and
/// `memory_slots`, the fields `stream::poll_compaction_status` diffs
/// against to detect a background compaction pass that finished after this
/// turn's own SSE stream closed.
pub async fn get_stats(app: &AppHandle, session_id: &str) -> Result<Value, String> {
    let client = ensure_client(app)?;
    client
        .get_json(&format!("/api/chat/{session_id}/stats"))
        .await
}

/// List sessions, translated to the goosed raw shape the frontend parses.
/// Capped at the 200 most recent — the sidebar's usable window.
pub async fn list(app: &AppHandle) -> Result<Vec<Value>, String> {
    list_with_limit(app, 200).await
}

/// [`list`] with an explicit daemon-side `limit`, for callers resolving one
/// specific session that may be older than the default 200-session window
/// (`windows::focus_or_open_session`'s cwd lookup retries through this).
pub async fn list_with_limit(app: &AppHandle, limit: u32) -> Result<Vec<Value>, String> {
    let client = ensure_client(app)?;
    let result = client.get_json(&format!("/api/chat/?limit={limit}")).await?;
    Ok(result
        .get("sessions")
        .and_then(|s| s.as_array())
        .map(|rows| rows.iter().map(translate_session_row).collect())
        .unwrap_or_default())
}

/// Pure: one BigTiny session row -> a goosed-style `session/list` object
/// (`parseSession` in the frontend reads exactly these keys). `_meta`
/// populates `SessionSummary.providerId`/`modelId` from the session's stored
/// metadata (`provider`/`model` — written by `set_session_provider`/
/// `rebind_session`'s `PATCH /config`), so the frontend's provider-restore
/// path (`chatStore.loadSession` → `findMatchingProvider`) has something to
/// match against; without this those fields were always `undefined` and the
/// whole restore was dead. Backwards compatible: `_meta` is additive.
pub(crate) fn translate_session_row(row: &Value) -> Value {
    let metadata: Value = row
        .get("metadata")
        .and_then(|m| m.as_str())
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Null);
    let provider_id = metadata.get("provider").and_then(|v| v.as_str());
    let model_id = metadata.get("model").and_then(|v| v.as_str());
    json!({
        "sessionId": row.get("id").and_then(|v| v.as_str()).unwrap_or(""),
        // BigTiny leaves `name` unset until the first turn completes (it
        // derives a title from the first message then — see
        // `bigtiny::agent::loop._derive_title`), so a brand-new session has
        // no name yet; "New Chat" is what the session list shows for it
        // until that auto-title lands.
        "title": row.get("name").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).unwrap_or("New Chat"),
        "cwd": metadata.get("cwd").and_then(|v| v.as_str()).unwrap_or(""),
        "updatedAt": row.get("updated_at").and_then(|v| v.as_str()).unwrap_or(""),
        "_meta": {
            "providerId": provider_id.unwrap_or(""),
            "modelId": model_id.unwrap_or(""),
        },
    })
}

/// Full message history for a session, oldest-first.
///
/// Long-timeout variant: `limit=10000` can mean a multi-megabyte payload on
/// a long-lived session, and the 10s default used to make resuming one fail
/// client-side while the daemon was still serializing.
async fn fetch_history(client: &BigTinyClient, session_id: &str) -> Result<Vec<Value>, String> {
    let result = client
        .get_json_long(&format!("/api/chat/{session_id}/history?limit=10000"))
        .await?;
    result
        .as_array()
        .cloned()
        .ok_or("BigTiny history was not a list".into())
}

/// Pure: displayable text of a stored message — plain string content, or the
/// concatenated `text` blocks of a `content_format == "blocks"` row.
pub(crate) fn extract_text(row: &Value) -> String {
    let content = row.get("content").and_then(|c| c.as_str()).unwrap_or("");
    if row.get("content_format").and_then(|f| f.as_str()) == Some("blocks") {
        if let Ok(Value::Array(blocks)) = serde_json::from_str::<Value>(content) {
            return blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("");
        }
    }
    content.to_string()
}

/// Resume a session: replay its history as the same `chat://*` events
/// goosed's `session/load` produces (the store buffers/renders them during
/// the call), then return the session info.
pub async fn load(app: &AppHandle, session_id: String, cwd: String) -> Result<SessionInfo, String> {
    let client = ensure_client(app)?;
    let rows = fetch_history(&client, &session_id).await?;

    for row in &rows {
        let role = row.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let text = extract_text(row);
        match role {
            "user" => {
                let _ = app.emit(
                    "chat://user-message",
                    json!({ "session_id": session_id, "text": text }),
                );
            }
            "assistant" => {
                if !text.is_empty() {
                    let _ = app.emit(
                        "chat://message-delta",
                        json!({ "session_id": session_id, "text": text }),
                    );
                }
                // Replay tool calls the assistant made this turn; their
                // results arrive as the following role="tool" rows.
                for tc in parse_tool_calls(row) {
                    let _ = app.emit(
                        "chat://tool-call",
                        json!({
                            "session_id": session_id,
                            "phase": "tool_call",
                            "update": tc,
                        }),
                    );
                }
            }
            "tool" => {
                let Some(tool_call_id) = row.get("tool_call_id").and_then(|v| v.as_str()) else {
                    continue;
                };
                let _ = app.emit(
                    "chat://tool-call",
                    json!({
                        "session_id": session_id,
                        "phase": "tool_call_update",
                        "update": {
                            "toolCallId": tool_call_id,
                            "status": "completed",
                            "rawOutput": crate::bigtiny::stream::truncate_for_ui(&text),
                        },
                    }),
                );
            }
            _ => {} // system rows are internal (persona/budget), never rendered
        }
    }

    Ok(session_info(app, session_id, cwd))
}

/// Pure: an assistant row's stored `tool_calls` JSON -> replayable
/// `chat://tool-call` update objects.
pub(crate) fn parse_tool_calls(row: &Value) -> Vec<Value> {
    let Some(raw) = row.get("tool_calls").and_then(|v| v.as_str()) else {
        return Vec::new();
    };
    let Ok(Value::Array(calls)) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    calls
        .iter()
        .filter_map(|tc| {
            let id = tc.get("id").and_then(|v| v.as_str())?;
            let name = tc
                .pointer("/function/name")
                .and_then(|v| v.as_str())
                .unwrap_or("tool");
            let args: Value = tc
                .pointer("/function/arguments")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(Value::Null);
            Some(json!({
                "toolCallId": id,
                "title": name,
                "kind": "execute",
                "rawInput": args,
                "_meta": { "goose": { "toolCall": { "toolName": name, "extensionName": "" } } },
            }))
        })
        .collect()
}

/// Fork a session, optionally truncated to the frontend's "keep the first N
/// UI bubbles" semantics (`truncate_from` = bubble count to keep, matching
/// goosed's conversation-index contract in `branch()`).
pub async fn fork(
    app: &AppHandle,
    session_id: String,
    cwd: String,
    truncate_from: Option<i64>,
) -> Result<SessionInfo, String> {
    let client = ensure_client(app)?;
    let at_message_id = match truncate_from {
        Some(keep) => {
            let rows = fetch_history(&client, &session_id).await?;
            truncate_target(&rows, keep)?
        }
        None => None,
    };
    let body = match at_message_id {
        Some(id) => json!({ "at_message_id": id }),
        None => json!({}),
    };
    let result = client
        .post_json(&format!("/api/chat/{session_id}/fork"), &body)
        .await?;
    let new_id = result
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or("BigTiny fork did not return a session id")?
        .to_string();
    let _ = app.emit("session://created", json!({ "sessionId": new_id }));
    Ok(session_info(app, new_id, cwd))
}

/// Pure: map "keep the first `keep` UI bubbles" onto the id of the last
/// history row belonging to bubble `keep`, for BigTiny's inclusive
/// `at_message_id` fork truncation.
///
/// Bubble simulation mirrors how the store renders a replay: each `user` row
/// opens a new user bubble; a contiguous run of `assistant`/`tool` rows after
/// it is ONE assistant bubble (deltas and tool cards append to the open
/// message until the next user turn); `system` rows are invisible and attach
/// to whatever bubble is open. Returns `Ok(None)` when `keep` covers the
/// whole history (fork copies everything).
///
/// `keep <= 0` is an **error**, not "copy everything": the daemon's fork has
/// no empty-copy form (`at_message_id` keeps every row up to and including
/// the cutoff, so any real message id keeps at least that message), and
/// mapping 0 to `None` used to silently duplicate the entire history.
pub(crate) fn truncate_target(rows: &[Value], keep: i64) -> Result<Option<String>, String> {
    if keep <= 0 {
        return Err(
            "branching needs at least one message kept — pick a message to branch from, or fork the whole chat"
                .into(),
        );
    }
    let mut bubble: i64 = 0;
    let mut in_assistant_run = false;
    let mut last_kept_id: Option<String> = None;
    let mut truncated = false;
    for row in rows {
        let role = row.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "user" => {
                bubble += 1;
                in_assistant_run = false;
            }
            "assistant" | "tool" if !in_assistant_run => {
                bubble += 1;
                in_assistant_run = true;
            }
            "assistant" | "tool" => {}
            _ => {} // system: attaches to the open bubble
        }
        if bubble <= keep {
            if let Some(id) = row.get("id").and_then(|v| v.as_str()) {
                last_kept_id = Some(id.to_string());
            }
        } else {
            truncated = true;
            break;
        }
    }
    if truncated {
        Ok(last_kept_id)
    } else {
        Ok(None) // keep >= total bubbles: copy the whole history
    }
}

/// Delete a session. The command layer owns the chat-folder cleanup and the
/// `session://deleted` event (shared with the goosed path).
pub async fn delete(app: &AppHandle, session_id: &str) -> Result<(), String> {
    let client = ensure_client(app)?;
    client.delete(&format!("/api/chat/{session_id}")).await?;
    Ok(())
}

/// User-initiated manual rename (`PATCH /api/chat/{id}` — the same route
/// BigTiny's own auto-title derivation writes through after the first turn).
/// A manual rename simply overwrites whatever name (auto-derived or not) was
/// there before; BigTiny never re-derives a title once a name is set.
pub async fn rename(app: &AppHandle, session_id: &str, name: &str) -> Result<(), String> {
    let client = ensure_client(app)?;
    client
        .patch_json(&format!("/api/chat/{session_id}"), &json!({ "name": name }))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, role: &str) -> Value {
        json!({ "id": id, "role": role, "content": "x" })
    }

    #[test]
    fn translate_session_row_reads_metadata_cwd() {
        let raw = json!({
            "id": "s1", "name": "My chat",
            "metadata": "{\"cwd\": \"C:/work\"}",
            "updated_at": "2026-07-23 10:00:00",
        });
        let t = translate_session_row(&raw);
        assert_eq!(t["sessionId"], "s1");
        assert_eq!(t["title"], "My chat");
        assert_eq!(t["cwd"], "C:/work");
        assert_eq!(t["updatedAt"], "2026-07-23 10:00:00");
    }

    #[test]
    fn translate_session_row_defaults_missing_fields() {
        let t = translate_session_row(&json!({ "id": "s2" }));
        assert_eq!(t["title"], "New Chat");
        assert_eq!(t["cwd"], "");
    }

    #[test]
    fn translate_session_row_defaults_empty_name_to_new_chat() {
        let t = translate_session_row(&json!({ "id": "s3", "name": "" }));
        assert_eq!(t["title"], "New Chat");
    }

    #[test]
    fn extract_text_plain_string() {
        assert_eq!(extract_text(&json!({ "content": "hello" })), "hello");
    }

    #[test]
    fn extract_text_blocks_concatenates_text_blocks_only() {
        let row = json!({
            "content": "[{\"type\":\"text\",\"text\":\"look \"},{\"type\":\"image\",\"data\":\"QUJD\"},{\"type\":\"text\",\"text\":\"here\"}]",
            "content_format": "blocks",
        });
        assert_eq!(extract_text(&row), "look here");
    }

    #[test]
    fn parse_tool_calls_maps_openai_shape() {
        let row = json!({
            "tool_calls": "[{\"id\":\"tc1\",\"type\":\"function\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}}]",
        });
        let calls = parse_tool_calls(&row);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["toolCallId"], "tc1");
        assert_eq!(calls[0]["title"], "shell");
        assert_eq!(calls[0]["rawInput"]["cmd"], "ls");
    }

    #[test]
    fn truncate_keeps_first_user_bubble_including_assistant_run() {
        // user(m0) | assistant(m1)+tool(m2)+assistant(m3) | user(m4) | assistant(m5)
        let rows = vec![
            row("m0", "user"),
            row("m1", "assistant"),
            row("m2", "tool"),
            row("m3", "assistant"),
            row("m4", "user"),
            row("m5", "assistant"),
        ];
        // keep 2 bubbles = first user turn + its whole assistant run
        assert_eq!(truncate_target(&rows, 2), Ok(Some("m3".to_string())));
        // keep 1 bubble = just the first user message
        assert_eq!(truncate_target(&rows, 1), Ok(Some("m0".to_string())));
        // keep everything -> no truncation marker
        assert_eq!(truncate_target(&rows, 4), Ok(None));
        assert_eq!(truncate_target(&rows, 99), Ok(None));
    }

    #[test]
    fn truncate_attaches_system_rows_to_open_bubble() {
        let rows = vec![
            row("m0", "user"),
            row("m1", "system"),
            row("m2", "assistant"),
            row("m3", "user"),
        ];
        // keep 2 = user + assistant run; the interleaved system row rides along
        assert_eq!(truncate_target(&rows, 2), Ok(Some("m2".to_string())));
    }

    /// Regression (815bugs #23): `keep <= 0` used to map to `None` — "copy
    /// the entire history" — so forking with a bubble count of 0 silently
    /// duplicated everything. The daemon has no empty-fork form, so this is
    /// an explicit error now.
    #[test]
    fn truncate_rejects_zero_and_negative_instead_of_full_copying() {
        let rows = vec![row("m0", "user"), row("m1", "assistant")];
        assert!(truncate_target(&rows, 0).is_err());
        assert!(truncate_target(&rows, -3).is_err());
    }
}
