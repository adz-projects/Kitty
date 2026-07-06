//! Ollama model-management commands (list/pull/delete) and the env-var helper
//! for the Advanced settings panel.

use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::config::env_helper;
use crate::lifecycle;
use crate::ollama;
use crate::state::AppState;

/// Shared by every command here plus `commands::setup::detect_dependencies`,
/// which needs the configured endpoint to run its Ollama version probe.
pub(super) fn ollama_base(app: &AppHandle) -> String {
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
