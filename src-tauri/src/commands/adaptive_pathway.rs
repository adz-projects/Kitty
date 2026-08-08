//! Commands for the behavioral-memory engine, now linked directly into the
//! BigTiny daemon (`plugins/adaptive-pathway_rust`) rather than a separate
//! sidecar process — see `crate::bigtiny::pathway` for the HTTP client and
//! `docs/ARCHITECTURE.md` for the current shape.
//!
//! Unlike the old sidecar (a small process Kitty could kill/respawn in
//! isolation in well under a second), the engine's `enabled` flag is read
//! once by BigTiny at process spawn (`BIGTINY_PATHWAY__ENABLED`) — there is
//! no live daemon-side reconfigure path for it. Toggling it here therefore
//! restarts the *entire* BigTiny daemon, briefly interrupting any in-flight
//! chat, not just Adaptive Pathway — a real behavioral change from the old
//! toggle's isolated restart, not an oversight.

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::bigtiny::client::ensure_client;
use crate::bigtiny::mcp::McpServer;
use crate::config;
use crate::state::AppState;

/// All beliefs currently held (Settings belief browser list).
#[tauri::command]
pub async fn get_pathway_beliefs(app: AppHandle) -> Result<Value, String> {
    let client = ensure_client(&app)?;
    crate::bigtiny::pathway::list_beliefs(&client).await
}

/// Belief counts by layer, for a lightweight Settings status readout.
#[tauri::command]
pub async fn get_pathway_stats(app: AppHandle) -> Result<Value, String> {
    let client = ensure_client(&app)?;
    crate::bigtiny::pathway::stats(&client).await
}

/// Belief browser's delete action. Suppresses (permanently, by default) and
/// tombstones the belief so extraction can't silently relearn it — see
/// `adaptive_pathway::store::suppressions::forget_belief_by_id`.
#[tauri::command]
pub async fn delete_pathway_belief(app: AppHandle, belief_id: String) -> Result<Value, String> {
    let client = ensure_client(&app)?;
    crate::bigtiny::pathway::delete_belief(&client, &belief_id).await
}

/// The incognito/pause toggle for one session: while paused, recall returns
/// nothing and nothing is written, for that session only.
#[tauri::command]
pub async fn set_pathway_session_paused(
    app: AppHandle,
    session_id: String,
    paused: bool,
) -> Result<Value, String> {
    let client = ensure_client(&app)?;
    crate::bigtiny::pathway::set_session_paused(&client, &session_id, paused).await
}

/// Connection status of the in-process `"pathway"` MCP server inside
/// BigTiny — whether the model can currently call `record`/`forget` as
/// tools. `Ok(None)` when the row doesn't exist yet (e.g. BigTiny not yet
/// synced, or the engine disabled and never registered).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct AdaptivePathwayMcpStatus {
    pub status: String,
    pub error_message: Option<String>,
    pub tool_count: usize,
}

#[tauri::command]
pub async fn get_adaptive_pathway_mcp_status(
    app: AppHandle,
) -> Result<Option<AdaptivePathwayMcpStatus>, String> {
    let client = ensure_client(&app)?;
    let servers = crate::bigtiny::mcp::list_servers(&client).await?;
    let Some(pathway) = servers.into_iter().find(|s| s.name == "pathway") else {
        return Ok(None);
    };
    Ok(Some(AdaptivePathwayMcpStatus {
        status: pathway.status.clone(),
        error_message: pathway.error_message.clone(),
        tool_count: pathway_tool_count(&client, &pathway).await,
    }))
}

/// Best-effort count of tools BigTiny has registered for the pathway server
/// (2 when connected: `record`, `forget`; 0 if tools are missing from the
/// live tool list despite a `connected` row).
async fn pathway_tool_count(
    client: &crate::bigtiny::client::BigTinyClient,
    pathway: &McpServer,
) -> usize {
    client
        .get_json(&format!("/api/mcp/servers/{}/tools", pathway.id))
        .await
        .ok()
        .and_then(|v| v.get("tools").and_then(|t| t.as_array()).map(|a| a.len()))
        .unwrap_or(0)
}

/// Enable/disable the engine. Persists config, restarts the BigTiny daemon
/// so `BIGTINY_PATHWAY__ENABLED` actually takes effect (see this module's
/// doc comment — there is no lighter-weight path), then re-syncs the
/// `"pathway"` MCP-server registration and the active provider the same way
/// any other daemon restart does.
#[tauri::command]
pub async fn set_adaptive_pathway_enabled(app: AppHandle, enabled: bool) -> Result<(), String> {
    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        cfg.adaptive_pathway_enabled = enabled;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    crate::commands::restart_backend(app).await
}
