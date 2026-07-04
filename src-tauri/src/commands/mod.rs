//! `#[tauri::command]` handlers — thin wrappers over the modules above. Every
//! command returns `Result<T, String>` with user-safe messages; details are
//! logged with `tracing`, not surfaced to the webview.

use std::path::PathBuf;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use crate::config::providers::{self, NetworkTier, ProviderProfile};
use crate::config::{self, env_helper, Config};
use crate::goosed::api;
use crate::lifecycle::{self, StackStatus};
use crate::state::AppState;
use crate::{hotkey, notifications, ollama, windows};

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
                notifications::notify_if_hidden(
                    &app_bg,
                    notifications::Event::TaskComplete,
                    "Goose finished",
                    "Your task is complete.",
                );
            }
            Err(message) => {
                let _ = app_bg.emit(
                    "chat://error",
                    json!({ "session_id": sid, "message": &message }),
                );
                notifications::notify_if_hidden(
                    &app_bg,
                    notifications::Event::TaskFailed,
                    "Goose ran into a problem",
                    &message,
                );
            }
        }
        // A finished turn clears any pending-approval tray state.
        notifications::set_tray_pending(&app_bg, false);
    });
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

/// Delete a session (ACP `session/delete`).
#[tauri::command]
pub async fn delete_session(app: AppHandle, session_id: String) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    client
        .request("session/delete", json!({ "sessionId": session_id }))
        .await?;
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
    restart_goosed(app).await
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

/// Toggle a built-in extension by add/remove on the active session.
#[tauri::command]
pub async fn set_extension_enabled(
    app: AppHandle,
    session_id: String,
    name: String,
    enabled: bool,
) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    let (method, params) = if enabled {
        (
            "_goose/unstable/session/extensions/add",
            json!({ "sessionId": session_id, "extension": { "type": "builtin", "name": name } }),
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
