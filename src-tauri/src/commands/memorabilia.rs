//! Commands for the declarative factual-memory engine (`memorabilia`),
//! linked into the BigTiny daemon (`plugins/memorabilia_rust`) alongside
//! adaptive-pathway. Exact parallel to `crate::commands::adaptive_pathway` —
//! see `crate::bigtiny::memorabilia` for the HTTP client.
//!
//! As with pathway, the engine's `enabled` flag is read once by BigTiny at
//! process spawn (`BIGTINY_MEMORABILIA__ENABLED`); there is no live
//! daemon-side reconfigure path, so `set_memorabilia_enabled` restarts the
//! whole BigTiny daemon to apply the change (briefly interrupting any
//! in-flight chat), then re-syncs the `"memorabilia"` MCP-server registration.

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::bigtiny::client::ensure_client;
use crate::bigtiny::mcp::McpServer;
use crate::config;
use crate::state::AppState;

/// Active memory items (Settings fact browser list).
#[tauri::command]
pub async fn get_memorabilia_items(app: AppHandle) -> Result<Value, String> {
    let client = ensure_client(&app)?;
    crate::bigtiny::memorabilia::list_items(&client).await
}

/// Counts for a lightweight Settings health readout.
#[tauri::command]
pub async fn get_memorabilia_stats(app: AppHandle) -> Result<Value, String> {
    let client = ensure_client(&app)?;
    crate::bigtiny::memorabilia::stats(&client).await
}

/// Fact browser's delete action. Suppresses (permanently) and tombstones the
/// item's supporting evidence so extraction can't silently relearn it — see
/// `memorabilia::privacy`'s `Engine::forget_item`.
#[tauri::command]
pub async fn delete_memorabilia_item(app: AppHandle, item_id: String) -> Result<Value, String> {
    let client = ensure_client(&app)?;
    crate::bigtiny::memorabilia::delete_item(&client, &item_id).await
}

/// The incognito/pause toggle for one session: while paused, recall injects
/// nothing and nothing is ingested, for that session only. Kitty drives this
/// together with the pathway pause from a single chat-header control.
#[tauri::command]
pub async fn set_memorabilia_session_paused(
    app: AppHandle,
    session_id: String,
    paused: bool,
) -> Result<Value, String> {
    let client = ensure_client(&app)?;
    crate::bigtiny::memorabilia::set_session_paused(&client, &session_id, paused).await
}

/// Connection status of the in-process `"memorabilia"` MCP server inside
/// BigTiny — whether the model can currently call `memorabilia_search` /
/// `memorabilia_read_item` as tools. `Ok(None)` when the row doesn't exist
/// yet (BigTiny not synced, or the engine disabled and never registered).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct MemorabiliaMcpStatus {
    pub status: String,
    pub error_message: Option<String>,
    pub tool_count: usize,
}

#[tauri::command]
pub async fn get_memorabilia_mcp_status(
    app: AppHandle,
) -> Result<Option<MemorabiliaMcpStatus>, String> {
    let client = ensure_client(&app)?;
    let servers = crate::bigtiny::mcp::list_servers(&client).await?;
    let Some(memorabilia) = servers.into_iter().find(|s| s.name == "memorabilia") else {
        return Ok(None);
    };
    Ok(Some(MemorabiliaMcpStatus {
        status: memorabilia.status.clone(),
        error_message: memorabilia.error_message.clone(),
        tool_count: memorabilia_tool_count(&client, &memorabilia).await,
    }))
}

/// Best-effort count of tools BigTiny has registered for the memorabilia
/// server (2 when connected: `memorabilia_search`, `memorabilia_read_item`;
/// 0 if tools are missing from the live tool list despite a `connected` row).
async fn memorabilia_tool_count(
    client: &crate::bigtiny::client::BigTinyClient,
    memorabilia: &McpServer,
) -> usize {
    client
        .get_json(&format!("/api/mcp/servers/{}/tools", memorabilia.id))
        .await
        .ok()
        .and_then(|v| v.get("tools").and_then(|t| t.as_array()).map(|a| a.len()))
        .unwrap_or(0)
}

/// Enable/disable the engine. Persists config, restarts the BigTiny daemon so
/// `BIGTINY_MEMORABILIA__ENABLED` takes effect (see this module's doc comment
/// — there is no lighter-weight path), then re-syncs the `"memorabilia"`
/// MCP-server registration the same way any other daemon restart does.
#[tauri::command]
pub async fn set_memorabilia_enabled(app: AppHandle, enabled: bool) -> Result<(), String> {
    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        cfg.memorabilia_enabled = enabled;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    crate::commands::restart_backend(app).await
}
