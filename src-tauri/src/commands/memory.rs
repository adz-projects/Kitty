//! Memory pre-flight telemetry — proxies BigTiny's daemon-global
//! `GET /api/memory/stats` to the settings pane. Read-only; powers the
//! "% of prompts with injected context" readout in Settings > Advanced.

use serde_json::Value;
use tauri::AppHandle;

use crate::bigtiny::client::ensure_client;

/// Global (all-session, process-lifetime) pre-flight memory recall counters:
/// `{ total_prompts, injected_prompts, injection_rate_pct }`. Polled ~every
/// 5s while the Advanced pane is open.
#[tauri::command]
pub async fn get_memory_stats(app: AppHandle) -> Result<Value, String> {
    let client = ensure_client(&app)?;
    let resp = client.get_json("/api/memory/stats").await?;
    Ok(resp)
}