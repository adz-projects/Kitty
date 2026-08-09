//! Ollama model-management commands (list/pull/delete) and the env-var helper
//! for the Advanced settings panel.

use serde_json::Value;
use tauri::{AppHandle, Manager};

#[cfg(windows)]
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

/// Best-effort context-length lookup for the Providers form's auto-suggest
/// (Round-6 Feature 1). `Ok(None)` (not `Err`) on any failure — this is a
/// suggestion, not a required value.
#[tauri::command]
pub async fn ollama_show_context_length(
    app: AppHandle,
    model: String,
) -> Result<Option<u32>, String> {
    Ok(ollama::show_model_context_length(&ollama_base(&app), &model).await)
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

// The OLLAMA_* env vars live in HKCU\Environment, so these two are Windows
// only (docs/ANDROID.md §2.5); `lib.rs` gates their handler entries to match.
#[cfg(windows)]
#[tauri::command]
pub fn read_ollama_env() -> Result<Vec<env_helper::EnvVar>, String> {
    Ok(env_helper::read_all())
}

#[cfg(windows)]
#[tauri::command]
pub fn set_ollama_env(name: String, value: Option<String>) -> Result<(), String> {
    env_helper::set(&name, value.as_deref())
}

/// Ensure Ollama is reachable, spawning it if down and installed (mirrors
/// `lifecycle::ollama_proc::ensure_running`, never kills anything). Unlike
/// `restart_ollama` this works even when Ollama isn't a process Kitty
/// already owns — used by Settings' "set up learning model" action and the
/// wizard's embedding step, both of which can run before `start_stack` had
/// any reason to start Ollama (e.g. an api-key chat provider that doesn't
/// otherwise need it, before adaptive-pathway's own need is provisioned).
#[tauri::command]
pub async fn ensure_ollama_running(app: AppHandle) -> Result<(), String> {
    let base = ollama_base(&app);
    // Don't trust the stale `child.is_some()` handle alone: a crashed Ollama
    // leaves a dead-but-Some` Child, and the old guard treated that as "still
    // running", permanently blocking respawn. Probe the actual process (and,
    // when owned, reap the dead handle) before deciding.
    let already_running = {
        let state = app.state::<AppState>();
        let mut ollama = state.ollama.lock().unwrap();
        let handle_alive = ollama
            .child
            .as_mut()
            .map(|c| c.try_wait().map(|s| s.is_none()).unwrap_or(false))
            .unwrap_or(false);
        if !handle_alive {
            // Clear the stale handle so `ensure_running` can respawn.
            ollama.child = None;
        }
        handle_alive
    };
    if already_running {
        // `ensure_running`'s probe-first path returns a *new*
        // ManagedProcess{owned: false} once it's up — overwriting
        // `state.ollama` with that here would leak the real child handle
        // (never killed on exit). Trust the existing owned handle instead.
        return Ok(());
    }
    let proc = lifecycle::ollama_proc::ensure_running(&base).await?;
    *app.state::<AppState>().ollama.lock().unwrap() = proc;
    Ok(())
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
