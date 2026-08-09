//! Scheduled task CRUD commands — see `config::scheduled_tasks` for the data
//! model and `lifecycle::spawn_scheduler_loop` for the fire logic. The
//! frontend computes `next_fire` (it owns the schedule-editing form); these
//! commands are a thin, dumb persistence layer, matching `commands::folders`.

use chrono::{DateTime, Local};
use tauri::{AppHandle, Emitter};

use crate::config;
use crate::config::scheduled_tasks::{Schedule, ScheduledTask};
use crate::state::AppState;

fn emit_changed(app: &AppHandle) {
    let _ = app.emit("scheduled_tasks://changed", ());
}

#[tauri::command]
pub fn list_scheduled_tasks(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<ScheduledTask>, String> {
    let cfg = state.config.lock().unwrap();
    Ok(cfg.scheduled_tasks.clone())
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub fn create_scheduled_task(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    name: String,
    prompt: String,
    cwd: Option<String>,
    model_id: Option<String>,
    schedule: Schedule,
    next_fire: DateTime<Local>,
) -> Result<ScheduledTask, String> {
    let name = name.trim().to_string();
    let prompt = prompt.trim().to_string();
    if name.is_empty() {
        return Err("Task name can't be empty.".into());
    }
    if prompt.is_empty() {
        return Err("Prompt can't be empty.".into());
    }
    let task = ScheduledTask {
        id: format!("task_{}", chrono::Utc::now().timestamp_millis()),
        name,
        prompt,
        cwd,
        // Empty string from an unset UI picker means "no override", not a
        // model literally named "".
        model_id: model_id.filter(|m| !m.trim().is_empty()),
        schedule,
        next_fire,
        enabled: true,
    };
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.scheduled_tasks.push(task.clone());
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    emit_changed(&app);
    Ok(task)
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub fn update_scheduled_task(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
    name: String,
    prompt: String,
    cwd: Option<String>,
    model_id: Option<String>,
    schedule: Schedule,
    next_fire: DateTime<Local>,
    enabled: bool,
) -> Result<(), String> {
    let name = name.trim().to_string();
    let prompt = prompt.trim().to_string();
    if name.is_empty() {
        return Err("Task name can't be empty.".into());
    }
    if prompt.is_empty() {
        return Err("Prompt can't be empty.".into());
    }
    {
        let mut cfg = state.config.lock().unwrap();
        let task = cfg
            .scheduled_tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or("task not found")?;
        task.name = name;
        task.prompt = prompt;
        task.cwd = cwd;
        task.model_id = model_id.filter(|m| !m.trim().is_empty());
        task.schedule = schedule;
        task.next_fire = next_fire;
        task.enabled = enabled;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    emit_changed(&app);
    Ok(())
}

#[tauri::command]
pub fn delete_scheduled_task(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.scheduled_tasks.retain(|t| t.id != id);
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    emit_changed(&app);
    Ok(())
}

#[tauri::command]
pub fn set_scheduled_task_enabled(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
    enabled: bool,
) -> Result<(), String> {
    {
        let mut cfg = state.config.lock().unwrap();
        let task = cfg
            .scheduled_tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or("task not found")?;
        task.enabled = enabled;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    emit_changed(&app);
    Ok(())
}
