//! MCP server management commands — the BigTiny-backed replacement for the
//! old goosed-path `commands::extensions` (which read/wrote goose's own
//! `config.yaml`) and `commands::replacement_mcp`. BigTiny's MCP servers are
//! daemon-global and managed live over REST (`bigtiny::mcp`), so every
//! command here just proxies to the running daemon — no config.yaml, no
//! restart required for add/edit/delete/toggle.

use crate::bigtiny::client::ensure_client;
use crate::bigtiny::mcp::{self, McpServer, McpServerPatch, McpServerSpec};
use crate::config;
use crate::state::AppState;

#[tauri::command]
pub async fn list_mcp_servers(app: tauri::AppHandle) -> Result<Vec<McpServer>, String> {
    let client = ensure_client(&app)?;
    mcp::list_servers(&client).await
}

#[tauri::command]
pub async fn add_mcp_server(app: tauri::AppHandle, spec: McpServerSpec) -> Result<String, String> {
    let client = ensure_client(&app)?;
    let id = mcp::create_server(&client, &spec).await?;
    if spec.enabled {
        // If the post-create auto-connect fails, roll the row back so we
        // don't leave a half-configured, never-connected server card behind
        // (mirrors the missing-cleanup gap in the old code).
        if let Err(e) = mcp::connect_server(&client, &id).await {
            let _ = mcp::delete_server(&client, &id).await;
            return Err(e);
        }
    }
    Ok(id)
}

#[tauri::command]
pub async fn update_mcp_server(
    app: tauri::AppHandle,
    id: String,
    patch: McpServerPatch,
) -> Result<McpServer, String> {
    let client = ensure_client(&app)?;
    mcp::update_server(&client, &id, &patch).await
}

#[tauri::command]
pub async fn delete_mcp_server(app: tauri::AppHandle, id: String) -> Result<(), String> {
    let client = ensure_client(&app)?;
    mcp::delete_server(&client, &id).await
}

#[tauri::command]
pub async fn set_mcp_server_enabled(
    app: tauri::AppHandle,
    id: String,
    enabled: bool,
) -> Result<McpServer, String> {
    let client = ensure_client(&app)?;
    mcp::update_server(
        &client,
        &id,
        &McpServerPatch {
            enabled: Some(enabled),
            ..Default::default()
        },
    )
    .await
}

#[tauri::command]
pub async fn connect_mcp_server(app: tauri::AppHandle, id: String) -> Result<(), String> {
    let client = ensure_client(&app)?;
    mcp::connect_server(&client, &id).await
}

/// Whether the bundled `kitty-wasm` server (sandboxed WebAssembly
/// Python/arbitrary-module execution — the Rust replacement for the retired
/// `wasm-math-mcp` Python plugin) is registered+enabled in BigTiny. No
/// credentials, so a plain toggle like `replacement_mcp` above.
#[tauri::command]
pub fn get_kitty_wasm_enabled(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.config.lock().unwrap().kitty_wasm_enabled)
}

#[tauri::command]
pub async fn set_kitty_wasm_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.kitty_wasm_enabled = enabled;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    mcp::ensure_builtin_servers(&app).await;
    Ok(())
}

/// Whether the bundled `visualizations` server is registered+enabled in
/// BigTiny. No credentials, so a plain toggle like `wasm_math_mcp` above.
#[tauri::command]
pub fn get_visualizations_enabled(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.config.lock().unwrap().visualizations_enabled)
}

#[tauri::command]
pub async fn set_visualizations_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.visualizations_enabled = enabled;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    mcp::ensure_builtin_servers(&app).await;
    Ok(())
}

/// Whether the bundled `kitty-tools` server — shell/workspace/file/word/
/// cache/scratchpad, plus Brave search and the 2 visualization tools gated
/// by their own flags — is registered+enabled in BigTiny. This flag alone
/// controls whether the whole process runs; `visualizations_enabled`/
/// `brave_mcp_search_enabled` separately control which tools it advertises
/// once running (see `bigtiny::mcp::ensure_builtin_servers`).
#[tauri::command]
pub fn get_kitty_tools_enabled(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.config.lock().unwrap().kitty_tools_enabled)
}

#[tauri::command]
pub async fn set_kitty_tools_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.kitty_tools_enabled = enabled;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    mcp::ensure_builtin_servers(&app).await;
    Ok(())
}

/// Whether the bundled `kitty-web` web-search/web-scrape server is
/// registered+enabled in BigTiny. No credentials, so a plain toggle like
/// `wasm_math_mcp` above.
#[tauri::command]
pub fn get_kitty_web_enabled(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.config.lock().unwrap().kitty_web_enabled)
}

#[tauri::command]
pub async fn set_kitty_web_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.kitty_web_enabled = enabled;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    mcp::ensure_builtin_servers(&app).await;
    Ok(())
}

/// Brave Search MCP status for Settings — `enabled` mirrors the user's
/// toggle intent, `configured` reports whether an API key is currently
/// stored in the keyring. The UI shows the API-key form whenever `!configured`,
/// regardless of `enabled`, since the two can never usefully disagree for
/// long (see `set_brave_mcp_search_enabled`).
#[derive(serde::Serialize)]
pub struct BraveMcpSearchStatus {
    pub enabled: bool,
    pub configured: bool,
}

#[tauri::command]
pub async fn get_brave_mcp_search_status(
    state: tauri::State<'_, AppState>,
) -> Result<BraveMcpSearchStatus, String> {
    let enabled = state.config.lock().unwrap().brave_mcp_search_enabled;
    let configured = config::providers::has_secret("brave-mcp-search");
    Ok(BraveMcpSearchStatus {
        enabled,
        configured,
    })
}

/// Store the Brave Search API key and enable the server in one step — the
/// only way to turn brave-mcp-search on. There is deliberately no
/// "just flip enabled=true" path: without a key the server can't do
/// anything, so enabling always means (re)configuring it fresh.
#[tauri::command]
pub async fn set_brave_mcp_search_api_key(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    api_key: String,
) -> Result<(), String> {
    let trimmed = api_key.trim();
    if trimmed.is_empty() {
        return Err("API key cannot be empty".to_string());
    }
    // Confirm the key actually works before storing it — otherwise a wrong
    // or mistyped key still renders a green "configured" checkbox and every
    // search silently comes back AUTH_ERROR with no indication why.
    mcp::validate_brave_api_key(trimmed).await?;
    config::providers::set_secret("brave-mcp-search", trimmed)?;
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.brave_mcp_search_enabled = true;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    mcp::ensure_builtin_servers(&app).await;
    Ok(())
}

/// Disabling always deletes the stored key (see `brave_mcp_search_enabled`'s
/// doc comment in `config/mod.rs`) — re-enabling therefore always goes
/// through `set_brave_mcp_search_api_key` and requires the user to type the
/// key again. There is no "disable but keep the key" — that's the entire
/// point of this flag's design.
#[tauri::command]
pub async fn set_brave_mcp_search_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    if enabled {
        return Err(
            "brave-mcp-search must be enabled via set_brave_mcp_search_api_key".to_string(),
        );
    }
    config::providers::delete_secret("brave-mcp-search");
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.brave_mcp_search_enabled = false;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    mcp::ensure_builtin_servers(&app).await;
    Ok(())
}
