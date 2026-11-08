//! Settings → Memorabilia → Overnight learning (docs/MEMORY_OVERNIGHT.md).
//!
//! The settings live in Kitty's config; `lifecycle::overnight` relays the
//! window to the engine and keeps the Windows wake-up task in step.

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::config::{self, OvernightSettings};
use crate::lifecycle::wake_task::{self, WakeTimers};
use crate::state::AppState;

/// Everything the panel shows.
#[derive(Debug, Serialize)]
pub struct OvernightView {
    pub settings: OvernightSettings,
    /// The engine's `GET /api/apps/me/overnight` body (`status`, `backlog`),
    /// or `null` when the engine could not be reached.
    pub engine: Value,
    pub wake_task_registered: bool,
    pub wake_timers: WakeTimers,
    /// Overnight learning exists on this platform (desktop).
    pub supported: bool,
}

#[tauri::command]
pub async fn get_overnight(app: AppHandle) -> Result<OvernightView, String> {
    let settings = app.state::<AppState>().config.lock().unwrap().overnight.clone();
    let supported = !cfg!(target_os = "android");
    let engine = match crate::bigtiny::client::ensure_client(&app) {
        Ok(c) => c.get_json("/api/apps/me/overnight").await.unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    let (registered, timers) = if supported {
        tauri::async_runtime::spawn_blocking(|| (wake_task::is_registered(), wake_task::wake_timers()))
            .await
            .unwrap_or_default()
    } else {
        (false, WakeTimers::default())
    };
    Ok(OvernightView {
        settings,
        engine,
        wake_task_registered: registered,
        wake_timers: timers,
        supported,
    })
}

fn valid_time(s: &str) -> bool {
    s.trim()
        .split_once(':')
        .and_then(|(h, m)| Some((h.parse::<u32>().ok()?, m.parse::<u32>().ok()?)))
        .is_some_and(|(h, m)| h < 24 && m < 60 && s.trim().len() == 5)
}

/// Save and apply. A wake-up task Windows refuses is reported, and the saved
/// settings say it is off.
#[tauri::command]
pub async fn set_overnight(
    app: AppHandle,
    mut settings: OvernightSettings,
) -> Result<OvernightSettings, String> {
    if !valid_time(&settings.start) || !valid_time(&settings.end) {
        return Err("Times must be written as HH:MM, for example 00:30.".into());
    }
    if settings.start.trim() == settings.end.trim() {
        return Err("The window must start and end at different times.".into());
    }
    settings.start = settings.start.trim().to_string();
    settings.end = settings.end.trim().to_string();
    let save = |app: &AppHandle, s: &OvernightSettings| -> Result<(), String> {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        cfg.overnight = s.clone();
        config::save(&cfg).map_err(|e| e.to_string())
    };
    save(&app, &settings)?;
    crate::lifecycle::overnight::apply_overnight(&app).await;
    if let Err(e) = crate::lifecycle::overnight::sync_wake_task(&app).await {
        if settings.wake {
            settings.wake = false;
            save(&app, &settings)?;
            return Err(e);
        }
        tracing::warn!("overnight: could not remove the wake-up task: {e}");
    }
    Ok(settings)
}

/// *Process now*: the engine runs the pass at its next sweep tick, without
/// waiting for the window or for the user to step away.
#[tauri::command]
pub async fn run_overnight_now(app: AppHandle) -> Result<(), String> {
    let client = crate::bigtiny::client::ensure_client(&app)?;
    client
        .post_json("/api/apps/me/overnight/run", &json!({}))
        .await
        .map(|_| ())
}

/// One screen-log row as written for labelling: `keep` is left `null` for the
/// user to fill in, and `scores` is an object rather than a JSON string.
fn label_line(row: &Value) -> Value {
    let scores = row
        .get("scores")
        .and_then(Value::as_str)
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or(Value::Null);
    json!({
        "text": row.get("text").cloned().unwrap_or(Value::Null),
        "heading": row.get("heading").cloned().unwrap_or(Value::Null),
        "source_type": row.get("source_type").cloned().unwrap_or(Value::Null),
        "source_name": row.get("source_name").cloned().unwrap_or(Value::Null),
        "question_set": row.get("question_set").cloned().unwrap_or(Value::Null),
        "scores": scores,
        "verdict": row.get("verdict").cloned().unwrap_or(Value::Null),
        "created_at": row.get("created_at").cloned().unwrap_or(Value::Null),
        "keep": Value::Null,
    })
}

/// Write the newest screened paragraphs to `path` as JSONL for labelling
/// (docs/MEMORY_OVERNIGHT.md, *Testing question sets*). Pre-filtered
/// paragraphs are left out unless `include_prefiltered`. Returns how many
/// lines were written.
#[tauri::command]
pub async fn save_screen_log(
    app: AppHandle,
    path: String,
    limit: Option<u32>,
    include_prefiltered: Option<bool>,
) -> Result<usize, String> {
    let client = crate::bigtiny::client::ensure_client(&app)?;
    let limit = limit.unwrap_or(1000).clamp(1, 5000);
    let body = client
        .get_json(&format!("/api/apps/me/memorabilia/screen-log?limit={limit}"))
        .await?;
    if let Some(err) = body.get("error").and_then(Value::as_str) {
        return Err(format!("The memory engine said: {err}"));
    }
    let with_prefiltered = include_prefiltered.unwrap_or(false);
    let lines: Vec<String> = body
        .get("rows")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter(|r| {
                    with_prefiltered
                        || !r
                            .get("verdict")
                            .and_then(Value::as_str)
                            .is_some_and(|v| v.starts_with("prefilter:"))
                })
                .map(|r| label_line(r).to_string())
                .collect()
        })
        .unwrap_or_default();
    let n = lines.len();
    let mut text = lines.join("\n");
    text.push('\n');
    tauri::async_runtime::spawn_blocking(move || std::fs::write(&path, text))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("Could not save the file: {e}"))?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_must_be_hh_mm() {
        assert!(valid_time("00:30"));
        assert!(valid_time("23:59"));
        assert!(!valid_time("24:00"));
        assert!(!valid_time("0:30"));
        assert!(!valid_time("noon"));
    }

    #[test]
    fn label_lines_leave_keep_empty_and_parse_scores() {
        let row = json!({
            "text": "Sloped faces absorb waves.", "heading": null,
            "source_type": "Scraped", "source_name": "example.org",
            "question_set": "durable-substantive-v1",
            "scores": "{\"durable\":0.81,\"substantive\":0.7}",
            "verdict": "kept", "created_at": "2026-11-07T10:00:00Z",
        });
        let l = label_line(&row);
        assert!(l["keep"].is_null());
        assert_eq!(l["scores"]["durable"], 0.81);
        assert_eq!(l["source_name"], "example.org");
    }
}
