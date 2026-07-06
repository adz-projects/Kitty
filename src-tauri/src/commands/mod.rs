//! `#[tauri::command]` handlers — thin wrappers over the modules above. Every
//! command returns `Result<T, String>` with user-safe messages; details are
//! logged with `tracing`, not surfaced to the webview.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use crate::config::providers::{self, NetworkTier, ProviderProfile};
use crate::config::{self, env_helper, Config};
use crate::goosed::api;
use crate::lifecycle::{self, StackStatus};
use crate::state::AppState;
use crate::{hotkey, notifications, ollama, windows, wizard};

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
        let changed =
            cur.hotkeys != config.hotkeys || cur.clipboard_hotkey != config.clipboard_hotkey;
        *cur = config.clone();
        changed
    };

    config::save(&config).map_err(|e| {
        tracing::error!("failed to save config: {e}");
        "Could not save settings to disk.".to_string()
    })?;

    // Let every window re-apply theme/background from the new config.
    let _ = app.emit("theme://changed", ());

    if hotkey_changed {
        if let Err(e) =
            hotkey::register(&app, &config.hotkeys, config.clipboard_hotkey.as_deref())
        {
            tracing::error!("re-register hotkey failed: {e}");
            return Err("Saved, but a new hotkey could not be registered.".into());
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

/// Open settings, optionally deep-linked to a section + highlighted element.
/// Async so window creation dispatches to the main thread (a sync command would
/// deadlock: it holds the main thread while `build()` needs it).
#[tauri::command]
pub async fn open_settings(
    app: AppHandle,
    section: Option<String>,
    highlight: Option<String>,
) -> Result<(), String> {
    windows::open_settings(&app, section, highlight).map_err(|e| e.to_string())
}

/// The settings deep-link target the window should navigate to on open.
#[tauri::command]
pub fn get_settings_target(state: tauri::State<'_, AppState>) -> Result<Option<Value>, String> {
    Ok(state.settings_target.lock().unwrap().clone())
}

// ============================ Phase 6: theming ============================

#[derive(Debug, Clone, Serialize)]
pub struct ThemeList {
    pub builtins: Vec<String>,
    pub user: Vec<String>,
}

/// Built-in theme names plus any user `.css` files in the themes folder.
#[tauri::command]
pub fn list_themes() -> Result<ThemeList, String> {
    let dir = config::themes_dir().map_err(|e| e.to_string())?;
    let mut user = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.to_ascii_lowercase().ends_with(".css") {
                user.push(name);
            }
        }
    }
    user.sort();
    Ok(ThemeList {
        builtins: vec!["default".into(), "dark".into()],
        user,
    })
}

/// Read a user theme's CSS text by filename (must live in the themes folder).
#[tauri::command]
pub fn read_user_theme(name: String) -> Result<String, String> {
    // Guard against path traversal — filename only.
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err("invalid theme name".into());
    }
    let path = config::themes_dir().map_err(|e| e.to_string())?.join(&name);
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("could not read theme {name}: {e}"))?;
    // Strip a leading UTF-8 BOM, which would otherwise break the first CSS rule.
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
}

/// Open the user themes folder in the file explorer.
#[tauri::command]
pub fn open_themes_folder(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let dir = config::themes_dir().map_err(|e| e.to_string())?;
    app.opener()
        .open_path(dir.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}

// ============================ Phase 7: wizard / setup ============================

/// Detect Ollama + Goose (presence, version, path).
#[tauri::command]
pub async fn detect_dependencies(app: AppHandle) -> Result<wizard::Detection, String> {
    let base = ollama_base(&app);
    Ok(wizard::detect(&base).await)
}

/// Download + launch a dependency's official installer (`ollama` / `goose`).
#[tauri::command]
pub async fn install_dependency(which: String) -> Result<(), String> {
    wizard::install(&which).await
}

/// Open the wizard in `"setup"` or `"repair"` mode.
#[tauri::command]
pub async fn open_wizard(app: AppHandle, mode: Option<String>) -> Result<(), String> {
    windows::open_wizard(&app, mode.as_deref().unwrap_or("setup")).map_err(|e| e.to_string())
}

/// The wizard launch mode the window should read on open.
#[tauri::command]
pub fn get_wizard_mode(state: tauri::State<'_, AppState>) -> Result<Option<String>, String> {
    Ok(state.wizard_mode.lock().unwrap().clone())
}

/// Mark first-run setup complete, then summon the overlay.
#[tauri::command]
pub async fn complete_setup(app: AppHandle) -> Result<(), String> {
    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        cfg.setup_completed = true;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    if let Some(win) = app.get_webview_window(windows::WIZARD) {
        let _ = win.hide();
    }
    windows::show_overlay(&app).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_autostart() -> Result<bool, String> {
    Ok(wizard::autostart_enabled())
}

#[tauri::command]
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    wizard::set_autostart(enabled)
}

/// Read an image file as a base64 data URL (for the background image, avoiding
/// asset-protocol scope config).
#[tauri::command]
pub fn read_image_data_url(path: String) -> Result<String, String> {
    use base64::Engine;
    let bytes = std::fs::read(&path).map_err(|e| format!("could not read image: {e}"))?;
    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png")
        .to_ascii_lowercase();
    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/png",
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{mime};base64,{b64}"))
}

/// Open the full window. Async so window creation dispatches to the main thread.
#[tauri::command]
pub async fn open_main(app: AppHandle) -> Result<(), String> {
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
    let env = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        config::providers::goosed_env(&cfg)
    };
    let handle = lifecycle::goosed::spawn(env).await?;
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

/// Prefix every no-explicit-folder session's private chat folder lives under
/// (Round-3 item 25). `delete_session` only ever removes a directory under
/// this prefix — never a user-chosen custom working directory.
pub const CHATS_DIR_NAME: &str = "chats";

/// A fresh, unique folder for a session with no explicit context folder, under
/// `%USERPROFILE%\Documents\Kitty\chats\<timestamp>-<short-rand>\`. Replaces
/// both the old chat-only Downloads default and the old shared agentic
/// `Documents/Goose` default: each such session now gets its own isolated
/// folder instead of sharing one across sessions (Round-3 item 25 — a real,
/// deliberate behavior change, not just a path rename).
fn new_private_chat_folder() -> PathBuf {
    use rand::Rng;
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let suffix: String = {
        let mut rng = rand::thread_rng();
        (0..6).map(|_| format!("{:x}", rng.gen_range(0u8..16))).collect()
    };
    dirs::document_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Kitty")
        .join(CHATS_DIR_NAME)
        .join(format!("{ts}-{suffix}"))
}

/// The working directory a new session starts in: the configured default
/// context folder, else a fresh private folder under `Documents/Kitty/chats/`
/// (created if missing) — same fallback for both agentic and chat-only modes.
fn resolve_cwd(app: &AppHandle) -> String {
    let configured = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        cfg.default_context_folder.clone()
    };
    let path = configured.map(PathBuf::from).unwrap_or_else(new_private_chat_folder);
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

/// Write a UTF-8 text file (Phase 11 ChatML export). The path comes from the
/// user's native save dialog.
#[tauri::command]
pub fn write_file(path: String, content: String) -> Result<(), String> {
    std::fs::write(&path, content).map_err(|e| format!("could not write {path}: {e}"))
}

/// Read a text file for inlining into a chat-only message (Phase 9). Rejects
/// binaries and files over the cap (default 200 KB).
#[tauri::command]
pub fn read_text_file(path: String, max_bytes: Option<usize>) -> Result<String, String> {
    let cap = max_bytes.unwrap_or(200 * 1024);
    let meta = std::fs::metadata(&path).map_err(|e| format!("could not open file: {e}"))?;
    if meta.len() as usize > cap {
        return Err(format!("File is too large to attach (> {} KB).", cap / 1024));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("could not read file: {e}"))?;
    String::from_utf8(bytes)
        .map_err(|_| "That looks like a binary file — only text can be attached here.".to_string())
}

/// A file attached to a chat, classified as UTF-8 text or binary (Round-2 item 13).
#[derive(Debug, Clone, Serialize)]
pub struct FileAttachment {
    pub name: String,
    /// `"text"` or `"binary"`.
    pub kind: String,
    /// Text content for `text`; a `data:<mime>;base64,…` URL for `binary`.
    pub content: String,
    pub mime: Option<String>,
}

/// Read a dropped file for attachment to ANY provider (Round-2 item 13): UTF-8
/// files come back as text; anything else as a base64 data URL. Binaries are no
/// longer rejected. Capped (default 25 MB — large enough for a typical photo)
/// so we don't inline huge payloads.
#[tauri::command]
pub fn read_file_any(path: String, max_bytes: Option<usize>) -> Result<FileAttachment, String> {
    let cap = max_bytes.unwrap_or(25 * 1024 * 1024);
    let name = std::path::Path::new(&path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&path)
        .to_string();
    let meta = std::fs::metadata(&path).map_err(|e| format!("could not open file: {e}"))?;
    if meta.len() as usize > cap {
        return Err(format!("File is too large to attach (> {} KB).", cap / 1024));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("could not read file: {e}"))?;
    match String::from_utf8(bytes) {
        Ok(text) => Ok(FileAttachment {
            name,
            kind: "text".into(),
            content: text,
            mime: Some("text/plain".into()),
        }),
        Err(e) => {
            use base64::Engine;
            let bytes = e.into_bytes();
            let ext = std::path::Path::new(&path)
                .extension()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let mime = match ext.as_str() {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "webp" => "image/webp",
                "pdf" => "application/pdf",
                _ => "application/octet-stream",
            };
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            Ok(FileAttachment {
                name,
                kind: "binary".into(),
                content: format!("data:{mime};base64,{b64}"),
                mime: Some(mime.to_string()),
            })
        }
    }
}

// ===================== Round-2 item 15: chat folders (app-side) =====================

/// App-side chat-folder state: the folder list + session→folder assignments.
#[derive(Debug, Clone, Serialize)]
pub struct FolderData {
    pub folders: Vec<String>,
    pub assignments: HashMap<String, String>,
}

#[tauri::command]
pub fn list_folders(state: tauri::State<'_, AppState>) -> Result<FolderData, String> {
    let cfg = state.config.lock().unwrap();
    Ok(FolderData {
        folders: cfg.folders.clone(),
        assignments: cfg.session_folders.clone(),
    })
}

#[tauri::command]
pub fn create_folder(state: tauri::State<'_, AppState>, name: String) -> Result<(), String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Folder name can’t be empty.".into());
    }
    let mut cfg = state.config.lock().unwrap();
    if !cfg.folders.iter().any(|f| f == &name) {
        cfg.folders.push(name);
    }
    config::save(&cfg).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn rename_folder(
    state: tauri::State<'_, AppState>,
    old: String,
    new: String,
) -> Result<(), String> {
    let new = new.trim().to_string();
    if new.is_empty() {
        return Err("Folder name can’t be empty.".into());
    }
    let mut cfg = state.config.lock().unwrap();
    for f in cfg.folders.iter_mut() {
        if *f == old {
            *f = new.clone();
        }
    }
    for v in cfg.session_folders.values_mut() {
        if *v == old {
            *v = new.clone();
        }
    }
    config::save(&cfg).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_folder(state: tauri::State<'_, AppState>, name: String) -> Result<(), String> {
    let mut cfg = state.config.lock().unwrap();
    cfg.folders.retain(|f| f != &name);
    cfg.session_folders.retain(|_, v| v != &name);
    config::save(&cfg).map_err(|e| e.to_string())
}

/// Assign a session to a folder, or `None` to move it back to Uncategorized.
#[tauri::command]
pub fn assign_session_folder(
    state: tauri::State<'_, AppState>,
    session_id: String,
    folder: Option<String>,
) -> Result<(), String> {
    let mut cfg = state.config.lock().unwrap();
    match folder {
        Some(f) if !f.trim().is_empty() => {
            cfg.session_folders.insert(session_id, f);
        }
        _ => {
            cfg.session_folders.remove(&session_id);
        }
    }
    config::save(&cfg).map_err(|e| e.to_string())
}

// ===================== Round-4: instant per-session mode toggle =====================

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
        let chats_root = dirs::document_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Kitty")
            .join(CHATS_DIR_NAME);
        let chats_root = chats_root.to_string_lossy().replace('\\', "/");
        let cwd_norm = cwd.replace('\\', "/");
        if cwd_norm.starts_with(&chats_root) {
            let _ = std::fs::remove_dir_all(&cwd_norm);
        }
    }
    Ok(())
}

/// Metadata about a dropped path (file vs. folder) for composer chips.
#[derive(Debug, Clone, Serialize)]
pub struct PathInfo {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub exists: bool,
}

/// Inspect dropped paths so the composer can show file/folder chips.
#[tauri::command]
pub fn inspect_paths(paths: Vec<String>) -> Result<Vec<PathInfo>, String> {
    Ok(paths
        .into_iter()
        .map(|p| {
            let path = PathBuf::from(&p);
            let meta = std::fs::metadata(&path);
            PathInfo {
                name: path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| p.clone()),
                is_dir: meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
                exists: meta.is_ok(),
                path: p,
            }
        })
        .collect())
}

/// Open a file/folder with the OS default handler (artifacts "Open").
#[tauri::command]
pub fn open_path(app: AppHandle, path: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_path(&path, None::<&str>)
        .map_err(|e| format!("could not open {path}: {e}"))
}

/// Reveal a file in its containing folder (artifacts "Show in Folder").
#[tauri::command]
pub fn reveal_path(app: AppHandle, path: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .reveal_item_in_dir(&path)
        .map_err(|e| format!("could not reveal {path}: {e}"))
}

// ============================ Phase 5: providers ============================

/// A provider profile plus derived fields the UI needs.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderView {
    #[serde(flatten)]
    pub profile: ProviderProfile,
    pub network_tier: NetworkTier,
    pub has_secret: bool,
    pub active: bool,
}

fn provider_views(cfg: &Config) -> Vec<ProviderView> {
    cfg.providers
        .iter()
        .map(|p| ProviderView {
            network_tier: p.network_tier(),
            has_secret: providers::has_secret(&p.id),
            active: cfg.active_provider_id.as_deref() == Some(&p.id),
            profile: p.clone(),
        })
        .collect()
}

/// List provider profiles with derived tier / secret / active flags.
#[tauri::command]
pub fn list_providers(state: tauri::State<'_, AppState>) -> Result<Vec<ProviderView>, String> {
    let cfg = state.config.lock().unwrap();
    Ok(provider_views(&cfg))
}

/// Create or update a provider profile. `secret`, when present, is stored in the
/// keyring only (never in config.json). Returns the saved profile (with id).
#[tauri::command]
pub fn upsert_provider(
    state: tauri::State<'_, AppState>,
    mut profile: ProviderProfile,
    secret: Option<String>,
) -> Result<ProviderProfile, String> {
    if profile.id.trim().is_empty() {
        profile.id = format!("prof_{}", chrono::Utc::now().timestamp_millis());
    }
    if profile.created_at.trim().is_empty() {
        profile.created_at = chrono::Utc::now().to_rfc3339();
    }
    if let Some(s) = secret {
        if !s.is_empty() {
            providers::set_secret(&profile.id, &s)?;
        }
    }
    let mut cfg = state.config.lock().unwrap();
    match cfg.providers.iter_mut().find(|p| p.id == profile.id) {
        Some(existing) => *existing = profile.clone(),
        None => cfg.providers.push(profile.clone()),
    }
    config::save(&cfg).map_err(|e| e.to_string())?;
    Ok(profile)
}

/// Delete a provider profile (and its keyring secret).
#[tauri::command]
pub fn delete_provider(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    providers::delete_secret(&id);
    let mut cfg = state.config.lock().unwrap();
    cfg.providers.retain(|p| p.id != id);
    if cfg.active_provider_id.as_deref() == Some(&id) {
        cfg.active_provider_id = None;
    }
    config::save(&cfg).map_err(|e| e.to_string())
}

/// Activate a provider profile (`None` = use goosed's own config). Persists the
/// choice and restarts goosed so the provider env takes effect.
#[tauri::command]
pub async fn activate_provider(app: AppHandle, id: Option<String>) -> Result<(), String> {
    // Capture the previously-active Ollama model so we can evict it after the
    // switch (Round-2 item 5) — read before we overwrite active_provider_id.
    let prev_ollama = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        providers::active_ollama_target(&cfg)
    };
    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        if let Some(ref pid) = id {
            if !cfg.providers.iter().any(|p| &p.id == pid) {
                return Err("no such provider profile".into());
            }
        }
        cfg.active_provider_id = id;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    restart_goosed(app.clone()).await?;

    // Tell the frontend to re-sync provider state immediately (Round-2 item 4) —
    // without this the UI drifts until the next session create/load or health tick.
    let _ = app.emit("provider://activated", ());

    // Warm the new local Ollama model + evict the old one in the background
    // (Round-2 item 5) — don't make the switch wait on model load.
    let new_ollama = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        providers::active_ollama_target(&cfg)
    };
    tauri::async_runtime::spawn(async move {
        if let Some((base, model)) = &new_ollama {
            ollama::keep_alive_load(base, model).await;
        }
        if let Some((base, model)) = &prev_ollama {
            if Some((base, model)) != new_ollama.as_ref().map(|(b, m)| (b, m)) {
                ollama::keep_alive_release(base, model).await;
            }
        }
    });
    Ok(())
}

// ============================ Phase 5: Ollama ============================

fn ollama_base(app: &AppHandle) -> String {
    app.state::<AppState>()
        .config
        .lock()
        .unwrap()
        .ollama_base_url
        .clone()
}

#[tauri::command]
pub async fn ollama_list_models(app: AppHandle) -> Result<Vec<Value>, String> {
    ollama::list_models(&ollama_base(&app)).await
}

#[tauri::command]
pub async fn ollama_delete_model(app: AppHandle, model: String) -> Result<(), String> {
    ollama::delete_model(&ollama_base(&app), &model).await
}

/// Start a model pull; returns a `pull_id` to correlate `ollama://pull-progress`
/// events. Multiple concurrent pulls are supported.
#[tauri::command]
pub fn ollama_pull_model(app: AppHandle, model: String) -> Result<String, String> {
    let pull_id = format!("pull_{}", chrono::Utc::now().timestamp_millis());
    let base = ollama_base(&app);
    let id = pull_id.clone();
    tauri::async_runtime::spawn(async move {
        ollama::pull_model(app, base, model, id).await;
    });
    Ok(pull_id)
}

// ============================ Phase 5: Ollama env helper ============================

#[tauri::command]
pub fn read_ollama_env() -> Result<Vec<env_helper::EnvVar>, String> {
    Ok(env_helper::read_all())
}

#[tauri::command]
pub fn set_ollama_env(name: String, value: Option<String>) -> Result<(), String> {
    env_helper::set(&name, value.as_deref())
}

/// Restart Ollama if we own the process (else the user must restart it).
#[tauri::command]
pub async fn restart_ollama(app: AppHandle) -> Result<(), String> {
    let base = ollama_base(&app);
    let owned = {
        let state = app.state::<AppState>();
        let mut ollama = state.ollama.lock().unwrap();
        if !ollama.owned {
            return Err("Ollama is running as a service or was started outside this app — restart it yourself.".into());
        }
        ollama.kill_if_owned();
        true
    };
    if owned {
        let proc = lifecycle::ollama_proc::ensure_running(&base).await?;
        *app.state::<AppState>().ollama.lock().unwrap() = proc;
    }
    Ok(())
}

// ============================ Phase 5: extensions ============================

/// List the active session's extensions (ACP unstable extension method).
#[tauri::command]
pub async fn list_extensions(app: AppHandle, session_id: String) -> Result<Vec<Value>, String> {
    let client = api::ensure_client(&app).await?;
    let result = client
        .request(
            "_goose/unstable/session/extensions/list",
            json!({ "sessionId": session_id }),
        )
        .await?;
    Ok(result
        .get("extensions")
        .and_then(|e| e.as_array())
        .cloned()
        .unwrap_or_else(|| result.as_array().cloned().unwrap_or_default()))
}

/// Toggle an extension by add/remove on the active session. `ext_type` and
/// `server` (only meaningful when `ext_type == "mcp"`) let re-enabling an mcp
/// extension send the correct tagged shape instead of always assuming builtin
/// (Round-3 item 15 — previously hardcoded `type:"builtin"`, which would send
/// the wrong shape for a custom mcp extension being turned back on).
#[tauri::command]
pub async fn set_extension_enabled(
    app: AppHandle,
    session_id: String,
    name: String,
    enabled: bool,
    ext_type: Option<String>,
    server: Option<Value>,
) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    let (method, params) = if enabled {
        let ty = ext_type.as_deref().unwrap_or("builtin");
        let extension = if ty == "mcp" {
            json!({ "type": "mcp", "server": server.unwrap_or(json!({ "name": name })) })
        } else {
            json!({ "type": ty, "name": name })
        };
        (
            "_goose/unstable/session/extensions/add",
            json!({ "sessionId": session_id, "extension": extension }),
        )
    } else {
        (
            "_goose/unstable/session/extensions/remove",
            json!({ "sessionId": session_id, "name": name }),
        )
    };
    client.request(method, params).await?;
    Ok(())
}

/// Add a custom stdio/mcp extension to the active session (Round-3 item 14).
#[tauri::command]
pub async fn add_extension(
    app: AppHandle,
    session_id: String,
    name: String,
    command: String,
    args: Vec<String>,
    env: Vec<String>,
) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    client
        .request(
            "_goose/unstable/session/extensions/add",
            json!({
                "sessionId": session_id,
                "extension": {
                    "type": "mcp",
                    "server": { "name": name, "command": command, "args": args, "env": env }
                }
            }),
        )
        .await?;
    Ok(())
}
