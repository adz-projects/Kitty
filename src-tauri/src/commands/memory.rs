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

/// Where the memory engines stand: whether the embedding model is on disk,
/// whether the engine loaded it, and whether each engine is running. Also
/// sent as `memory://status` when it changes.
#[tauri::command]
pub async fn get_memory_status(
    app: AppHandle,
) -> Result<crate::lifecycle::memory::MemoryStatus, String> {
    crate::lifecycle::memory::apply_memory_plugins(&app).await;
    let state = tauri::Manager::state::<crate::state::AppState>(&app);
    let status = state.memory_status.lock().unwrap().clone();
    Ok(status)
}
