//! Deep research commands. A run is started by sending a `/deepresearch`
//! message (`send_prompt`'s `research`); these read and stop runs.

use serde_json::Value;
use tauri::AppHandle;

use crate::bigtiny::client::ensure_client;
use crate::bigtiny::research;

/// One run: its plan, progress, coverage and steps.
#[tauri::command]
pub async fn get_research(app: AppHandle, research_id: String) -> Result<Value, String> {
    let client = ensure_client(&app)?;
    research::get(&client, &research_id).await.map_err(|e| {
        tracing::warn!("get_research {research_id}: {e}");
        "Could not read the research run.".to_string()
    })
}

/// A run's ledger, entry by entry.
#[tauri::command]
pub async fn list_research_entries(app: AppHandle, research_id: String) -> Result<Value, String> {
    let client = ensure_client(&app)?;
    research::entries(&client, &research_id).await.map_err(|e| {
        tracing::warn!("list_research_entries {research_id}: {e}");
        "Could not read the research ledger.".to_string()
    })
}

/// Kitty's runs, newest first; one chat's when `session_id` is given.
#[tauri::command]
pub async fn list_research(app: AppHandle, session_id: Option<String>) -> Result<Value, String> {
    let client = ensure_client(&app)?;
    research::list(&client, session_id.as_deref())
        .await
        .map_err(|e| {
            tracing::warn!("list_research: {e}");
            "Could not list research runs.".to_string()
        })
}

/// Stop a run. What it found so far stays readable.
#[tauri::command]
pub async fn cancel_research(app: AppHandle, research_id: String) -> Result<(), String> {
    let client = ensure_client(&app)?;
    research::cancel(&client, &research_id).await.map_err(|e| {
        tracing::warn!("cancel_research {research_id}: {e}");
        "Could not stop the research run.".to_string()
    })
}
