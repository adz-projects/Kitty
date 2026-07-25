//! Session lifecycle commands: create/send/cancel/approve/resume/fork/delete,
//! plus the mode-override and private-chat-folder helpers that only exist to
//! support them.

mod config;
mod crud;
mod prompt;

pub use config::*;
pub use crud::*;
pub use prompt::*;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffortOption {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingEffort {
    pub current_value: String,
    pub options: Vec<EffortOption>,
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
        .unwrap_or_else(|| {
            dirs::document_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Kitty")
        })
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
        (0..6)
            .map(|_| format!("{:x}", rng.gen_range(0u8..16)))
            .collect()
    };
    base.join(CHATS_DIR_NAME).join(format!("{ts}-{suffix}"))
}

/// The working directory a new session starts in: a fresh per-chat folder under
/// the (configurable) chats base, created if missing. Same for both modes.
/// `create_dir_all` runs on a blocking thread — this is user-triggered
/// (every "New Session"), so a slow disk shouldn't stall the tokio worker
/// other requests are running on.
async fn resolve_cwd(app: &AppHandle) -> String {
    let path = new_chat_folder(&chats_base_dir(app));
    let path_for_blocking = path.clone();
    let _ = tokio::task::spawn_blocking(move || std::fs::create_dir_all(&path_for_blocking)).await;
    path.to_string_lossy().replace('\\', "/")
}
