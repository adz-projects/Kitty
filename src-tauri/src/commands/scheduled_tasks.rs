//! Scheduled tasks, run by the engine's own scheduler (`/api/schedules`).
//!
//! Kitty used to fire these itself, from a loop that only ran while Kitty was
//! open (#69, #80). The engine keeps running without Kitty, so the schedules
//! live there now: these commands are a thin mapping between the settings
//! form's shape and the engine's, and `lifecycle::app_events` reports how
//! runs went. Tasks saved by an older Kitty are moved over once, at the first
//! attach ([`migrate_config_tasks`]).
//!
//! A task runs on the card it was given (else the default card when it
//! fires), with that card's system prompt, in its own folder, and waits at
//! most 10 minutes for a tool approval nobody may be around to give
//! (decision #7).

use chrono::{DateTime, Local};
use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use crate::bigtiny::client::{ensure_client, BigTinyClient};
use crate::config::scheduled_tasks::{Schedule, ScheduledTask};
use crate::state::AppState;

/// How long a scheduled run waits for an approval (decision #7).
const RUN_APPROVAL_TIMEOUT_SECS: i64 = 600;

fn emit_changed(app: &AppHandle) {
    let _ = app.emit("scheduled_tasks://changed", ());
}

/// A task as the settings page shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TaskView {
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub cwd: Option<String>,
    /// The card it runs on; `None` = the default card when it fires.
    pub provider_id: Option<String>,
    pub schedule: Schedule,
    /// When it next runs, if it will.
    pub next_fire: Option<String>,
    pub enabled: bool,
    pub last_run_at: Option<String>,
    /// `running`, `completed`, `completed_with_denied_tools` or `failed`.
    pub last_status: Option<String>,
    /// The chat the last run happened in, to open it.
    pub last_session_id: Option<String>,
}

/// An engine schedule row as a [`TaskView`]. Only the kinds Kitty creates
/// are shown; a `cron` schedule another tool made for this app is left alone.
fn view_of(row: &Value) -> Option<TaskView> {
    let s = |k: &str| {
        row.get(k)
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let schedule = match row.get("kind").and_then(|k| k.as_str())? {
        "once" => Schedule::OneShot,
        "interval" => Schedule::Recurring {
            interval_secs: row.get("interval_secs").and_then(|v| v.as_u64())?,
        },
        _ => return None,
    };
    Some(TaskView {
        id: s("id")?,
        name: s("name").unwrap_or_default(),
        prompt: s("prompt").unwrap_or_default(),
        cwd: s("cwd"),
        provider_id: s("provider_id"),
        next_fire: s("next_run_at").or_else(|| s("run_at")),
        schedule,
        enabled: row.get("enabled").and_then(|v| v.as_i64()).unwrap_or(0) != 0,
        last_run_at: s("last_run_at"),
        last_status: s("last_status"),
        last_session_id: s("last_session_id"),
    })
}

/// The engine-side fields for a task's timing. `first_fire` is when it
/// should (first) run.
fn timing(schedule: &Schedule, first_fire: DateTime<Local>) -> Value {
    match schedule {
        Schedule::OneShot => json!({
            "kind": "once",
            "run_at": first_fire.to_rfc3339(),
        }),
        Schedule::Recurring { interval_secs } => json!({
            "kind": "interval",
            "interval_secs": interval_secs,
            "first_run_at": first_fire.to_rfc3339(),
        }),
    }
}

/// The engine-side fields for the card a task runs on: the card and its
/// model, and its system prompt. No card: the default card when it fires,
/// with the built-in prompt.
fn card_fields(app: &AppHandle, provider_id: Option<&str>) -> Result<Value, String> {
    let card = match provider_id.filter(|p| !p.is_empty()) {
        Some(id) => {
            let state = app.state::<AppState>();
            let cfg = state.config.lock().unwrap();
            let card = cfg
                .providers
                .iter()
                .find(|p| p.id == id && p.is_usable())
                .cloned()
                .ok_or("That provider can't be used; choose another.")?;
            Some(card)
        }
        None => None,
    };
    Ok(match card {
        Some(card) => json!({
            "provider_id": card.id,
            "model": card.models.first(),
            "system_prompt": crate::config::providers::system_prompt_for(&card),
        }),
        None => json!({
            "provider_id": null,
            "model": null,
            "system_prompt": crate::config::providers::DEFAULT_SYSTEM_PROMPT,
        }),
    })
}

fn merge(into: &mut Value, from: Value) {
    if let (Some(a), Value::Object(b)) = (into.as_object_mut(), from) {
        a.extend(b);
    }
}

/// A folder for a task's runs when none was chosen: its own, under the chats
/// base, so its runs' files are kept and reachable.
async fn task_folder(app: &AppHandle, cwd: Option<String>) -> Result<String, String> {
    match cwd.filter(|c| !c.trim().is_empty()) {
        Some(c) => Ok(c.replace('\\', "/")),
        None => Ok(crate::commands::fresh_chat_folder(app)
            .await?
            .to_string_lossy()
            .replace('\\', "/")),
    }
}

#[tauri::command]
pub async fn list_scheduled_tasks(app: AppHandle) -> Result<Vec<TaskView>, String> {
    let client = ensure_client(&app)?;
    let v = client.get_json("/api/schedules").await?;
    Ok(v.get("schedules")
        .and_then(|s| s.as_array())
        .map(|rows| rows.iter().filter_map(view_of).collect())
        .unwrap_or_default())
}

#[tauri::command]
pub async fn create_scheduled_task(
    app: AppHandle,
    name: String,
    prompt: String,
    cwd: Option<String>,
    provider_id: Option<String>,
    schedule: Schedule,
    next_fire: DateTime<Local>,
) -> Result<String, String> {
    let (name, prompt) = (name.trim().to_string(), prompt.trim().to_string());
    if name.is_empty() {
        return Err("Task name can't be empty.".into());
    }
    if prompt.is_empty() {
        return Err("Prompt can't be empty.".into());
    }
    let mut body = json!({
        "name": name,
        "prompt": prompt,
        "cwd": task_folder(&app, cwd).await?,
        "hitl_timeout_secs": RUN_APPROVAL_TIMEOUT_SECS,
        "enabled": true,
    });
    merge(&mut body, timing(&schedule, next_fire));
    merge(&mut body, card_fields(&app, provider_id.as_deref())?);
    let client = ensure_client(&app)?;
    let created = client.post_json("/api/schedules", &body).await?;
    emit_changed(&app);
    Ok(created
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string())
}

/// Save an edited task. The engine keeps the next run where it was unless
/// the timing itself changed (#69).
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn update_scheduled_task(
    app: AppHandle,
    id: String,
    name: String,
    prompt: String,
    cwd: Option<String>,
    provider_id: Option<String>,
    schedule: Schedule,
    next_fire: DateTime<Local>,
    enabled: bool,
) -> Result<(), String> {
    let mut body = json!({
        "name": name.trim(),
        "prompt": prompt.trim(),
        "enabled": enabled,
    });
    if let Some(cwd) = cwd.filter(|c| !c.trim().is_empty()) {
        body["cwd"] = json!(cwd.replace('\\', "/"));
    }
    merge(&mut body, timing(&schedule, next_fire));
    merge(&mut body, card_fields(&app, provider_id.as_deref())?);
    let client = ensure_client(&app)?;
    client
        .patch_json(&format!("/api/schedules/{id}"), &body)
        .await?;
    emit_changed(&app);
    Ok(())
}

#[tauri::command]
pub async fn delete_scheduled_task(app: AppHandle, id: String) -> Result<(), String> {
    ensure_client(&app)?
        .delete(&format!("/api/schedules/{id}"))
        .await?;
    emit_changed(&app);
    Ok(())
}

#[tauri::command]
pub async fn set_scheduled_task_enabled(
    app: AppHandle,
    id: String,
    enabled: bool,
) -> Result<(), String> {
    ensure_client(&app)?
        .patch_json(
            &format!("/api/schedules/{id}"),
            &json!({ "enabled": enabled }),
        )
        .await?;
    emit_changed(&app);
    Ok(())
}

/// Run a task now. Returns the chat the run happens in, which can be opened
/// straight away to watch it.
#[tauri::command]
pub async fn run_scheduled_task_now(app: AppHandle, id: String) -> Result<String, String> {
    let v = ensure_client(&app)?
        .post_json(&format!("/api/schedules/{id}/run_now"), &json!({}))
        .await?;
    emit_changed(&app);
    v.get("session_id")
        .and_then(|s| s.as_str())
        .map(str::to_string)
        .ok_or_else(|| "The engine did not say where the run is happening.".to_string())
}

/// A task's past runs, newest first.
#[tauri::command]
pub async fn scheduled_task_runs(app: AppHandle, id: String) -> Result<Value, String> {
    let v = ensure_client(&app)?
        .get_json(&format!("/api/schedules/{id}/runs"))
        .await?;
    Ok(v.get("runs").cloned().unwrap_or(Value::Array(vec![])))
}

/// Move tasks saved by an older Kitty (kept in its config and fired by its
/// own loop) into the engine, once. A task pinned to a model moves onto the
/// card with that model, if there is one. The config list is cleared only
/// after every task made it across.
pub(crate) async fn migrate_config_tasks(app: &AppHandle) {
    let tasks: Vec<ScheduledTask> = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        if cfg.scheduled_tasks_migrated {
            return;
        }
        cfg.scheduled_tasks.clone()
    };
    let Ok(client) = ensure_client(app) else {
        return;
    };
    let mut all_moved = true;
    for task in &tasks {
        if let Err(e) = migrate_one(app, &client, task).await {
            tracing::warn!(
                "could not move scheduled task {} to the engine: {e}",
                task.id
            );
            all_moved = false;
        }
    }
    if !all_moved {
        return;
    }
    let state = app.state::<AppState>();
    let mut cfg = state.config.lock().unwrap();
    cfg.scheduled_tasks.clear();
    cfg.scheduled_tasks_migrated = true;
    if let Err(e) = crate::config::save(&cfg) {
        tracing::warn!("could not save the moved scheduled tasks: {e}");
    }
}

async fn migrate_one(
    app: &AppHandle,
    client: &BigTinyClient,
    task: &ScheduledTask,
) -> Result<(), String> {
    let card = task.model_id.as_deref().and_then(|model| {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        cfg.providers
            .iter()
            .find(|p| p.is_usable() && p.models.iter().any(|m| m == model))
            .map(|p| p.id.clone())
    });
    // A one-shot whose time has passed still runs once, at the next start.
    let mut body = json!({
        "name": task.name,
        "prompt": task.prompt,
        "cwd": task_folder(app, task.cwd.clone()).await?,
        "hitl_timeout_secs": RUN_APPROVAL_TIMEOUT_SECS,
        "enabled": task.enabled,
    });
    merge(&mut body, timing(&task.schedule, task.next_fire));
    merge(&mut body, card_fields(app, card.as_deref())?);
    client.post_json("/api/schedules", &body).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_rows_map_to_the_settings_shape() {
        let row = json!({
            "id": "j1", "name": "Hourly", "prompt": "check", "kind": "interval",
            "interval_secs": 3600, "enabled": 1, "next_run_at": "2026-10-01T10:00:00Z",
            "provider_id": "p1", "cwd": "C:/t", "last_status": "failed", "last_session_id": "job_1",
        });
        let v = view_of(&row).unwrap();
        assert_eq!(
            v.schedule,
            Schedule::Recurring {
                interval_secs: 3600
            }
        );
        assert_eq!(v.next_fire.as_deref(), Some("2026-10-01T10:00:00Z"));
        assert_eq!(v.provider_id.as_deref(), Some("p1"));
        assert!(v.enabled);
        assert_eq!(v.last_session_id.as_deref(), Some("job_1"));

        let once =
            json!({"id": "j2", "kind": "once", "run_at": "2026-10-02T09:00:00Z", "enabled": 0});
        let v = view_of(&once).unwrap();
        assert_eq!(v.schedule, Schedule::OneShot);
        assert_eq!(v.next_fire.as_deref(), Some("2026-10-02T09:00:00Z"));
        assert!(!v.enabled);

        assert!(view_of(&json!({"id": "c", "kind": "cron", "cron": "0 9 * * *"})).is_none());
    }

    #[test]
    fn timing_maps_onto_the_engine_kinds() {
        let at = Local::now();
        assert_eq!(timing(&Schedule::OneShot, at)["kind"], "once");
        let t = timing(&Schedule::Recurring { interval_secs: 60 }, at);
        assert_eq!(t["kind"], "interval");
        assert_eq!(t["interval_secs"], 60);
        assert!(t["first_run_at"].is_string());
    }
}
