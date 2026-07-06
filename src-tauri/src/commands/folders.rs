//! App-side chat-folder commands (Round-2 item 15) — a local session-id →
//! folder-name mapping layered on top of goosed's own `session/list`.

use std::collections::HashMap;

use serde::Serialize;

use crate::config;
use crate::state::AppState;

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
