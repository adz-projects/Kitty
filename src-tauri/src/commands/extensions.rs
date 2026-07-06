//! Extension (MCP/builtin) management commands for the active session.

use serde_json::{json, Value};
use tauri::AppHandle;

use crate::goosed::api;

/// List the active session's extensions (ACP unstable extension method).
#[tauri::command]
pub async fn list_extensions(app: AppHandle, session_id: String) -> Result<Vec<Value>, String> {
    let client = api::ensure_client(&app).await?;
    let result = client
        .request(
            "_goose/unstable/session/extensions/list",
            json!({ "sessionId": session_id }),
        )
        .await?;
    Ok(result
        .get("extensions")
        .and_then(|e| e.as_array())
        .cloned()
        .unwrap_or_else(|| result.as_array().cloned().unwrap_or_default()))
}

/// Toggle an extension by add/remove on the active session. `ext_type` and
/// `server` (only meaningful when `ext_type == "mcp"`) let re-enabling an mcp
/// extension send the correct tagged shape instead of always assuming builtin
/// (Round-3 item 15 — previously hardcoded `type:"builtin"`, which would send
/// the wrong shape for a custom mcp extension being turned back on).
#[tauri::command]
pub async fn set_extension_enabled(
    app: AppHandle,
    session_id: String,
    name: String,
    enabled: bool,
    ext_type: Option<String>,
    server: Option<Value>,
) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    let (method, params) = if enabled {
        let ty = ext_type.as_deref().unwrap_or("builtin");
        let extension = if ty == "mcp" {
            json!({ "type": "mcp", "server": server.unwrap_or(json!({ "name": name })) })
        } else {
            json!({ "type": ty, "name": name })
        };
        (
            "_goose/unstable/session/extensions/add",
            json!({ "sessionId": session_id, "extension": extension }),
        )
    } else {
        (
            "_goose/unstable/session/extensions/remove",
            json!({ "sessionId": session_id, "name": name }),
        )
    };
    client.request(method, params).await?;
    Ok(())
}

/// Add a custom stdio/mcp extension to the active session (Round-3 item 14).
#[tauri::command]
pub async fn add_extension(
    app: AppHandle,
    session_id: String,
    name: String,
    command: String,
    args: Vec<String>,
    env: Vec<String>,
) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    client
        .request(
            "_goose/unstable/session/extensions/add",
            json!({
                "sessionId": session_id,
                "extension": {
                    "type": "mcp",
                    "server": { "name": name, "command": command, "args": args, "env": env }
                }
            }),
        )
        .await?;
    Ok(())
}
