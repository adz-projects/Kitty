//! Session lifecycle commands: create/send/cancel/approve/resume/fork/delete,
//! plus the mode-override and private-chat-folder helpers that only exist to
//! support them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use crate::config;
use crate::config::providers;
use crate::goosed::api;
use crate::notifications;
use crate::state::AppState;

/// Store the active session (raw JSON) so the full window can adopt it on Expand.
/// Emits `session://active` so an *already-open* main window re-adopts the newly
/// handed-off session (its mount-time `getActiveSession` only runs once).
#[tauri::command]
pub fn set_active_session(app: AppHandle, info: Value) -> Result<(), String> {
    *app.state::<AppState>().active_session.lock().unwrap() = Some(info.clone());
    let _ = app.emit("session://active", info);
    Ok(())
}

/// Read the active session, if any (the full window calls this on mount).
#[tauri::command]
pub fn get_active_session(state: tauri::State<'_, AppState>) -> Result<Option<Value>, String> {
    Ok(state.active_session.lock().unwrap().clone())
}

/// Details returned when a session is created, for the chat UI.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub cwd: String,
    pub current_mode: String,
    pub available_modes: Vec<ModeInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModeInfo {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// Prefix every no-explicit-folder session's private chat folder lives under
/// (Round-3 item 25). `delete_session` only ever removes a directory under
/// this prefix — never a user-chosen custom working directory.
pub const CHATS_DIR_NAME: &str = "chats";

/// Base directory that holds every chat's own context folder. The user's choice
/// (`default_context_folder`, set in Settings) when non-empty, else the default
/// `~/Documents/Kitty`. Each chat then lives in `<base>/chats/<id>/`, so the
/// setting is a *base for per-chat folders*, not one shared working directory.
fn chats_base_dir(app: &AppHandle) -> PathBuf {
    let configured = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        cfg.default_context_folder.clone()
    };
    configured
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::document_dir().unwrap_or_else(|| PathBuf::from(".")).join("Kitty"))
}

/// A fresh per-chat folder `<base>/chats/<timestamp>-<short-rand>/`. The
/// `<timestamp>-<rand>` is the chat's own id — goose's session id isn't known
/// until `session/new` returns (the cwd is passed *into* it), so a client-side
/// id names the folder instead; `session/list.cwd` maps back to it later.
fn new_chat_folder(base: &Path) -> PathBuf {
    use rand::Rng;
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let suffix: String = {
        let mut rng = rand::thread_rng();
        (0..6).map(|_| format!("{:x}", rng.gen_range(0u8..16))).collect()
    };
    base.join(CHATS_DIR_NAME).join(format!("{ts}-{suffix}"))
}

/// The working directory a new session starts in: a fresh per-chat folder under
/// the (configurable) chats base, created if missing. Same for both modes.
fn resolve_cwd(app: &AppHandle) -> String {
    let path = new_chat_folder(&chats_base_dir(app));
    let _ = std::fs::create_dir_all(&path);
    path.to_string_lossy().replace('\\', "/")
}

/// Start a new goosed session (ACP `session/new`). Connects the ACP client on
/// first use. An explicit `cwd` (e.g. a dropped folder) overrides the default.
#[tauri::command]
pub async fn new_session(app: AppHandle, cwd: Option<String>) -> Result<SessionInfo, String> {
    let client = api::ensure_client(&app).await?;
    let cwd = match cwd {
        Some(c) if !c.trim().is_empty() => {
            let _ = std::fs::create_dir_all(&c);
            c.replace('\\', "/")
        }
        _ => resolve_cwd(&app),
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
    // and a BRAVE_API_KEY — see docs/acp-protocol.md.)
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

    Ok(SessionInfo {
        session_id,
        cwd,
        current_mode,
        available_modes,
    })
}

fn parse_mode(v: &Value) -> ModeInfo {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    ModeInfo {
        id: s("id"),
        name: s("name"),
        description: s("description"),
    }
}

/// An image attached to a chat turn (Round-3 item 17). `data_url` is a
/// `data:<mime>;base64,<...>` string as produced by `read_file_any`.
#[derive(Debug, Clone, Deserialize)]
pub struct ImageAttachment {
    pub mime: String,
    pub data_url: String,
}

/// Send a user turn (ACP `session/prompt`). Returns immediately; streamed
/// output arrives via `chat://*` events, and completion via `chat://complete`.
/// `images`, when present, are appended as native ACP image content blocks
/// (`{type:"image", data, mimeType}`, confirmed live — see acp-protocol.md)
/// instead of relying on a filesystem tool to open a path — this is what fixes
/// the "file not found" failure untrusted/remote providers hit on a bare path
/// reference (Round-3 item 17).
#[tauri::command]
pub async fn send_prompt(
    app: AppHandle,
    session_id: String,
    text: String,
    images: Option<Vec<ImageAttachment>>,
) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    let app_bg = app.clone();
    let sid = session_id.clone();
    let mut prompt = vec![json!({ "type": "text", "text": text })];
    for img in images.unwrap_or_default() {
        // Strip a "data:<mime>;base64," prefix if present; ACP wants raw base64.
        let data = img
            .data_url
            .split_once(",")
            .map(|(_, b64)| b64)
            .unwrap_or(&img.data_url);
        prompt.push(json!({ "type": "image", "data": data, "mimeType": img.mime }));
    }
    tauri::async_runtime::spawn(async move {
        let res = client
            .request(
                "session/prompt",
                json!({ "sessionId": sid, "prompt": prompt }),
            )
            .await;
        match res {
            Ok(result) => {
                let _ = app_bg.emit(
                    "chat://complete",
                    json!({ "session_id": sid, "result": result }),
                );
                notifications::notify_if_hidden(
                    &app_bg,
                    notifications::Event::TaskComplete,
                    "Kitty finished",
                    "Your task is complete.",
                );
                providers::emit_health_from_send_result(&app_bg, true);
            }
            Err(message) => {
                let _ = app_bg.emit(
                    "chat://error",
                    json!({ "session_id": sid, "message": &message }),
                );
                notifications::notify_if_hidden(
                    &app_bg,
                    notifications::Event::TaskFailed,
                    "Kitty ran into a problem",
                    &message,
                );
                providers::emit_health_from_send_result(&app_bg, false);
            }
        }
        // A finished turn clears any pending-approval tray state.
        notifications::set_tray_pending(&app_bg, false);
    });
    Ok(())
}

/// Cancel the in-flight turn for a session (ACP `session/cancel` notification).
/// goosed resolves the pending prompt with a `cancelled` stop reason.
#[tauri::command]
pub async fn cancel_prompt(app: AppHandle, session_id: String) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    client.notify("session/cancel", json!({ "sessionId": session_id }));
    Ok(())
}

/// Respond to a deferred tool-approval prompt. `option_id` = the chosen ACP
/// option (e.g. `allow_once`, `reject_once`); `None` cancels.
#[tauri::command]
pub async fn respond_permission(
    app: AppHandle,
    tool_call_id: String,
    option_id: Option<String>,
) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    let id = client
        .take_permission(&tool_call_id)
        .await
        .ok_or("that approval request is no longer pending")?;

    let outcome = match option_id {
        Some(opt) => json!({ "outcome": { "outcome": "selected", "optionId": opt } }),
        None => json!({ "outcome": { "outcome": "cancelled" } }),
    };
    client.respond(id, outcome);
    notifications::set_tray_pending(&app, false);
    Ok(())
}

/// Switch the session's approval mode (`auto` / `approve` / `smart_approve`).
#[tauri::command]
pub async fn set_mode(app: AppHandle, session_id: String, mode_id: String) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    client
        .request(
            "session/set_mode",
            json!({ "sessionId": session_id, "modeId": mode_id }),
        )
        .await?;
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

    Ok(SessionInfo {
        session_id,
        cwd,
        current_mode,
        available_modes,
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
        .request("session/fork", json!({ "sessionId": session_id, "cwd": cwd }))
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
    Ok(SessionInfo {
        session_id: new_id,
        cwd,
        current_mode,
        available_modes,
    })
}

/// Get a session's persisted chat/agentic mode override, if any (`None` =
/// follow the active provider's `tools_enabled` default).
#[tauri::command]
pub fn get_session_mode(
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> Result<Option<String>, String> {
    let cfg = state.config.lock().unwrap();
    Ok(cfg.session_modes.get(&session_id).cloned())
}

/// Set (or clear, via `None`) a session's mode override.
#[tauri::command]
pub fn set_session_mode(
    state: tauri::State<'_, AppState>,
    session_id: String,
    mode: Option<String>,
) -> Result<(), String> {
    let mut cfg = state.config.lock().unwrap();
    match mode {
        Some(m) if !m.trim().is_empty() => {
            cfg.session_modes.insert(session_id, m);
        }
        _ => {
            cfg.session_modes.remove(&session_id);
        }
    }
    config::save(&cfg).map_err(|e| e.to_string())
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
    if let Some(cwd) = cwd {
        // Only remove a folder that sits under the chats base's `chats/` dir
        // (a Kitty-created per-chat folder) — never a user's own directory.
        let chats_root = chats_base_dir(&app).join(CHATS_DIR_NAME);
        let chats_root = format!("{}/", chats_root.to_string_lossy().replace('\\', "/"));
        let cwd_norm = cwd.replace('\\', "/");
        if cwd_norm.starts_with(&chats_root) {
            let _ = std::fs::remove_dir_all(&cwd_norm);
        }
    }
    Ok(())
}
