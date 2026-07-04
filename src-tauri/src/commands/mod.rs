//! `#[tauri::command]` handlers — thin wrappers over the modules above. Every
//! command returns `Result<T, String>` with user-safe messages; details are
//! logged with `tracing`, not surfaced to the webview.

use std::path::PathBuf;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use crate::config::{self, Config};
use crate::goosed::api;
use crate::lifecycle::{self, StackStatus};
use crate::state::AppState;
use crate::{hotkey, windows};

/// Read the current app config.
#[tauri::command]
pub fn get_config(state: tauri::State<'_, AppState>) -> Result<Config, String> {
    Ok(state.config.lock().unwrap().clone())
}

/// Replace + persist the app config. Re-registers the hotkey if it changed.
#[tauri::command]
pub fn set_config(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    config: Config,
) -> Result<(), String> {
    let hotkey_changed = {
        let mut cur = state.config.lock().unwrap();
        let changed = cur.hotkey != config.hotkey;
        *cur = config.clone();
        changed
    };

    config::save(&config).map_err(|e| {
        tracing::error!("failed to save config: {e}");
        "Could not save settings to disk.".to_string()
    })?;

    if hotkey_changed {
        if let Err(e) = hotkey::register(&app, &config.hotkey) {
            tracing::error!("re-register hotkey failed: {e}");
            return Err("Saved, but the new hotkey could not be registered.".into());
        }
    }
    Ok(())
}

/// Show/hide the overlay from the frontend.
#[tauri::command]
pub fn toggle_overlay(app: AppHandle) -> Result<(), String> {
    windows::toggle_overlay(&app).map_err(|e| e.to_string())
}

/// Hide the overlay (Escape handler in the overlay UI calls this).
#[tauri::command]
pub fn hide_overlay(app: AppHandle) -> Result<(), String> {
    windows::hide_overlay(&app).map_err(|e| e.to_string())
}

/// Open settings, optionally deep-linked to a section (Phase 5 wires targets).
#[tauri::command]
pub fn open_settings(app: AppHandle, section: Option<String>) -> Result<(), String> {
    windows::open_settings(&app, section).map_err(|e| e.to_string())
}

/// Open the full window.
#[tauri::command]
pub fn open_main(app: AppHandle) -> Result<(), String> {
    windows::open_main(&app).map_err(|e| e.to_string())
}

/// Current stack status (frontend also listens to `stack://status`).
#[tauri::command]
pub fn get_stack_status(state: tauri::State<'_, AppState>) -> Result<StackStatus, String> {
    Ok(*state.stack_status.lock().unwrap())
}

/// Restart the goosed child (kills our owned process, respawns). "Fix this" and
/// the degraded-state panel call this.
#[tauri::command]
pub async fn restart_goosed(app: AppHandle) -> Result<(), String> {
    {
        let state = app.state::<AppState>();
        state.goosed.lock().unwrap().process.kill_if_owned();
    }
    // Drop the stale ACP connection so the next session reconnects.
    {
        let state = app.state::<AppState>();
        *state.acp.lock().await = None;
    }
    let handle = lifecycle::goosed::spawn().await?;
    let state = app.state::<AppState>();
    *state.goosed.lock().unwrap() = handle;
    Ok(())
}

/// Store the active session (raw JSON) so the full window can adopt it on Expand.
#[tauri::command]
pub fn set_active_session(state: tauri::State<'_, AppState>, info: Value) -> Result<(), String> {
    *state.active_session.lock().unwrap() = Some(info);
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

/// The working directory a new session starts in: the configured default
/// context folder, else `%USERPROFILE%\Documents\Goose` (created if missing).
fn resolve_cwd(app: &AppHandle) -> String {
    let configured = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        cfg.default_context_folder.clone()
    };
    let path = configured.map(PathBuf::from).unwrap_or_else(|| {
        dirs::document_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Goose")
    });
    let _ = std::fs::create_dir_all(&path);
    path.to_string_lossy().replace('\\', "/")
}

/// Start a new goosed session (ACP `session/new`). Connects the ACP client on
/// first use.
#[tauri::command]
pub async fn new_session(app: AppHandle) -> Result<SessionInfo, String> {
    let client = api::ensure_client(&app).await?;
    let cwd = resolve_cwd(&app);
    let result = client
        .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
        .await?;

    let session_id = result
        .get("sessionId")
        .and_then(|v| v.as_str())
        .ok_or("goosed did not return a session id")?
        .to_string();

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

fn parse_mode(v: &Value) -> ModeInfo {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    ModeInfo {
        id: s("id"),
        name: s("name"),
        description: s("description"),
    }
}

/// Send a user turn (ACP `session/prompt`). Returns immediately; streamed
/// output arrives via `chat://*` events, and completion via `chat://complete`.
#[tauri::command]
pub async fn send_prompt(app: AppHandle, session_id: String, text: String) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    let app_bg = app.clone();
    let sid = session_id.clone();
    tauri::async_runtime::spawn(async move {
        let res = client
            .request(
                "session/prompt",
                json!({
                    "sessionId": sid,
                    "prompt": [{ "type": "text", "text": text }]
                }),
            )
            .await;
        match res {
            Ok(result) => {
                let _ = app_bg.emit(
                    "chat://complete",
                    json!({ "session_id": sid, "result": result }),
                );
            }
            Err(message) => {
                let _ = app_bg.emit(
                    "chat://error",
                    json!({ "session_id": sid, "message": message }),
                );
            }
        }
    });
    Ok(())
}
