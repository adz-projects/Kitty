//! Session create/list/resume/fork/delete — the CRUD half of session
//! lifecycle commands.

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use crate::config;
use crate::goosed::api;
use crate::state::AppState;

use super::{chats_base_dir, parse_thinking_effort, resolve_cwd, ThinkingEffort, CHATS_DIR_NAME};

/// Details returned when a session is created, for the chat UI.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub cwd: String,
    pub current_mode: String,
    pub available_modes: Vec<ModeInfo>,
    /// `None` when the active model doesn't support effort control at all —
    /// see `parse_thinking_effort`'s doc comment (Round-7 Feature).
    pub thinking_effort: Option<ThinkingEffort>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModeInfo {
    pub id: String,
    pub name: String,
    pub description: String,
}

fn parse_mode(v: &Value) -> ModeInfo {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    ModeInfo {
        id: s("id"),
        name: s("name"),
        description: s("description"),
    }
}

/// Start a new goosed session (ACP `session/new`). Connects the ACP client on
/// first use. An explicit `cwd` (e.g. a dropped folder) overrides the default.
#[tauri::command]
pub async fn new_session(app: AppHandle, cwd: Option<String>) -> Result<SessionInfo, String> {
    let client = api::ensure_client(&app).await?;
    let cwd = match cwd {
        Some(c) if !c.trim().is_empty() => {
            let c_for_blocking = c.clone();
            let _ =
                tokio::task::spawn_blocking(move || std::fs::create_dir_all(&c_for_blocking)).await;
            c.replace('\\', "/")
        }
        _ => resolve_cwd(&app).await,
    };
    let result = client
        .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
        .await?;

    let session_id = result
        .get("sessionId")
        .and_then(|v| v.as_str())
        .ok_or("goosed did not return a session id")?
        .to_string();

    // Web search + artifacts should work in EVERY session regardless of the
    // provider's chat-only flag (Round-2 item 14a). The `computercontroller`
    // builtin is keyless and provides web search/fetch; best-effort + idempotent.
    // (Dedicated Brave search additionally needs the mcp-brave-search extension
    // and a BRAVE_API_KEY — see docs/acp-protocol.md.) Deliberately awaited
    // (not fire-and-forget): a spawned task racing against a fast
    // Delete-then-New-Chat could still be in flight against a session whose
    // directory the delete had already torn down, surfacing as an "Internal
    // error" toast with no actionable cause. The ~5-20ms extra latency here is
    // negligible next to the round trip `new_session` already blocks on.
    let _ = client
        .request(
            "_goose/unstable/session/extensions/add",
            json!({
                "sessionId": &session_id,
                "extension": { "type": "builtin", "name": "computercontroller" }
            }),
        )
        .await;

    let current_mode = result
        .pointer("/modes/currentModeId")
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        .to_string();
    let available_modes = result
        .pointer("/modes/availableModes")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(parse_mode).collect())
        .unwrap_or_default();

    // Cross-window live-update (Round-4 item 6): overlay and main each own an
    // independent zustand store, so nothing else tells the other window's
    // session list/recents dropdown a new session now exists.
    let _ = app.emit("session://created", json!({ "sessionId": session_id }));

    let thinking_effort = parse_thinking_effort(&result);

    Ok(SessionInfo {
        session_id,
        cwd,
        current_mode,
        available_modes,
        thinking_effort,
    })
}

/// Add one recipe-declared extension to a live session, best-effort — mirrors
/// `new_session`'s own `computercontroller` add above. Real Goose recipe
/// extension types (`stdio`/`builtin`/`platform`/`streamable_http`/`frontend`/
/// `inline_python`) don't line up with what ACP's `extensions/add` accepts
/// (`builtin`/`platform`/`mcp`, confirmed in `docs/acp-protocol.md`), so
/// `stdio` maps to the ACP `mcp` shape (env resolved to literal `KEY=VALUE`
/// strings from Kitty's own process env — never goosed's — matching the
/// confirmed `server.env` bare-string-array shape) and `builtin`/`platform`
/// pass straight through. The remaining three have no ACP equivalent at all —
/// silently skipped, never a hard failure, since an extension type ACP can't
/// represent must not break a recipe invocation.
#[tauri::command]
pub async fn add_recipe_extension(
    app: AppHandle,
    session_id: String,
    extension: crate::config::recipes::RecipeExtension,
) -> Result<(), String> {
    let payload = match extension.ext_type.as_str() {
        "builtin" => json!({ "type": "builtin", "name": extension.name }),
        "platform" => json!({ "type": "platform", "name": extension.name }),
        "stdio" => {
            let env: Vec<String> = extension
                .env_keys
                .iter()
                .filter_map(|k| std::env::var(k).ok().map(|v| format!("{k}={v}")))
                .collect();
            json!({
                "type": "mcp",
                "server": {
                    "name": extension.name,
                    "command": extension.cmd.clone().unwrap_or_default(),
                    "args": extension.args,
                    "env": env,
                },
            })
        }
        _ => return Ok(()),
    };
    let client = api::ensure_client(&app).await?;
    let _ = client
        .request(
            "_goose/unstable/session/extensions/add",
            json!({ "sessionId": session_id, "extension": payload }),
        )
        .await;
    Ok(())
}

/// List past sessions (raw ACP session objects; the frontend parses them).
#[tauri::command]
pub async fn list_sessions(app: AppHandle) -> Result<Vec<Value>, String> {
    let client = api::ensure_client(&app).await?;
    let result = client.request("session/list", json!({})).await?;
    Ok(result
        .get("sessions")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default())
}

/// Resume a session (ACP `session/load`). The conversation replays as
/// `chat://*` events during the call; returns the session's mode info.
#[tauri::command]
pub async fn load_session(
    app: AppHandle,
    session_id: String,
    cwd: String,
) -> Result<SessionInfo, String> {
    let client = api::ensure_client(&app).await?;
    let result = client
        .request(
            "session/load",
            json!({ "sessionId": session_id, "cwd": cwd, "mcpServers": [] }),
        )
        .await?;

    let current_mode = result
        .pointer("/modes/currentModeId")
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        .to_string();
    let available_modes = result
        .pointer("/modes/availableModes")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(parse_mode).collect())
        .unwrap_or_default();

    let thinking_effort = parse_thinking_effort(&result);

    Ok(SessionInfo {
        session_id,
        cwd,
        current_mode,
        available_modes,
        thinking_effort,
    })
}

/// Fork a session (ACP `session/fork`), optionally truncating the copy to a
/// branch point. Powers "Branch from here" and "Regenerate" (Phase 9).
#[tauri::command]
pub async fn fork_session(
    app: AppHandle,
    session_id: String,
    cwd: String,
    truncate_from: Option<i64>,
) -> Result<SessionInfo, String> {
    let client = api::ensure_client(&app).await?;
    let result = client
        .request(
            "session/fork",
            json!({ "sessionId": session_id, "cwd": cwd }),
        )
        .await?;
    let new_id = result
        .get("sessionId")
        .and_then(|v| v.as_str())
        .ok_or("fork did not return a session id")?
        .to_string();

    if let Some(n) = truncate_from {
        let _ = client
            .request(
                "_goose/unstable/session/conversation/truncate",
                json!({ "sessionId": new_id, "truncateFrom": n }),
            )
            .await;
    }

    let current_mode = result
        .pointer("/modes/currentModeId")
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        .to_string();
    let available_modes = result
        .pointer("/modes/availableModes")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(parse_mode).collect())
        .unwrap_or_default();
    let _ = app.emit("session://created", json!({ "sessionId": new_id }));
    let thinking_effort = parse_thinking_effort(&result);
    Ok(SessionInfo {
        session_id: new_id,
        cwd,
        current_mode,
        available_modes,
        thinking_effort,
    })
}

/// Delete a session (ACP `session/delete`). If `cwd` sits under the private
/// `Documents/Kitty/chats/` prefix (i.e. it was never an explicit user-chosen
/// working directory — see `resolve_cwd`), also remove that directory. The
/// prefix check is a hard safety gate: a custom/explicit folder is never
/// touched (Round-3 item 25).
#[tauri::command]
pub async fn delete_session(
    app: AppHandle,
    session_id: String,
    cwd: Option<String>,
) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    client
        .request("session/delete", json!({ "sessionId": session_id }))
        .await?;
    // Belt-and-suspenders alongside `forActive`'s stale-event guard in
    // chatStore.ts: without this, a deleted session id could linger in
    // `in_flight_sessions` (set by `send_prompt` in prompt.rs) until its
    // in-flight turn's own cleanup runs, however long that takes.
    app.state::<AppState>()
        .in_flight_sessions
        .lock()
        .unwrap()
        .remove(&session_id);
    if let Some(cwd) = cwd {
        // Only remove a folder that sits under the chats base's `chats/` dir
        // (a Kitty-created per-chat folder) — never a user's own directory.
        let chats_root = chats_base_dir(&app).join(CHATS_DIR_NAME);
        let chats_root = format!("{}/", chats_root.to_string_lossy().replace('\\', "/"));
        let cwd_norm = cwd.replace('\\', "/");
        if cwd_norm.starts_with(&chats_root) {
            let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&cwd_norm)).await;
        }
    }
    // Cross-window live-update, mirroring `session://created` (Round-4 item 6)
    // — without this, another window's sidebar/recents keeps showing this
    // session until it happens to refresh for some other reason (confirmed
    // real gap: `regenerate()`'s background cleanup of the superseded session
    // it forked away from has no other way to reach a different window).
    let _ = app.emit("session://deleted", json!({ "sessionId": session_id }));
    Ok(())
}

/// Delete every session (Settings → General "Clear all chat history" — a
/// standalone destructive action, unrelated to provider switching). Loops
/// `session/delete` per id since goosed has no bulk method, reusing
/// `delete_session`'s exact "only remove a folder under the chats-root
/// prefix" safety gate for each one's working directory. Also clears
/// `session_folders`/`session_modes` (app-side organization that can't refer
/// to a now-deleted session) and the active-session pointer.
#[tauri::command]
pub async fn clear_all_sessions(app: AppHandle) -> Result<usize, String> {
    let client = api::ensure_client(&app).await?;
    let sessions = {
        let result = client.request("session/list", json!({})).await?;
        result
            .get("sessions")
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_default()
    };

    let chats_root = chats_base_dir(&app).join(CHATS_DIR_NAME);
    let chats_root = format!("{}/", chats_root.to_string_lossy().replace('\\', "/"));

    let mut deleted = 0usize;
    let mut last_err: Option<String> = None;
    for s in &sessions {
        let Some(sid) = s.get("sessionId").and_then(|v| v.as_str()) else {
            continue;
        };
        match client
            .request("session/delete", json!({ "sessionId": sid }))
            .await
        {
            Ok(_) => {
                deleted += 1;
                if let Some(cwd) = s.get("cwd").and_then(|v| v.as_str()) {
                    let cwd_norm = cwd.replace('\\', "/");
                    if cwd_norm.starts_with(&chats_root) {
                        let _ =
                            tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&cwd_norm))
                                .await;
                    }
                }
            }
            Err(e) => last_err = Some(e), // keep going; one bad id shouldn't abort the rest
        }
    }

    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        cfg.session_folders.clear();
        cfg.session_modes.clear();
        config::save(&cfg).map_err(|e| e.to_string())?;
        *state.active_session.lock().unwrap() = None;
    }
    // Deliberately not re-emitting `session://active` with a null payload here
    // — `onActiveSession`'s only consumer (`main/App.tsx`) assumes a real
    // `SessionInfo` and dereferences `info.session_id` unconditionally, so a
    // null payload would throw there. `session://cleared` below is what
    // already-open windows react to instead (SessionList/chatStore both blank
    // their own state directly, the same pattern `handOffToMain` uses).
    let _ = app.emit("session://cleared", json!({ "deleted": deleted }));

    match last_err {
        Some(e) if deleted == 0 => Err(e),
        _ => Ok(deleted),
    }
}
