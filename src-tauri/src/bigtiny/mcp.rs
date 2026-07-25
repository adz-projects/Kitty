//! MCP server management for the BigTiny backend — thin wrappers over
//! `/api/mcp/servers`, plus the idempotent upsert that keeps Kitty's two
//! bundled plugins (replacement-mcp, adaptive-pathway) registered against the
//! current install's bundled exe paths. Mirrors `bigtiny::providers`'
//! sync-over-REST approach: no daemon restart needed for any of this.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use tauri::{AppHandle, Manager};

use crate::bigtiny::client::{ensure_client, BigTinyClient};
use crate::state::AppState;

/// A BigTiny MCP server row, with the JSON-string `args`/`env` columns
/// parsed into structured data for the frontend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServer {
    pub id: String,
    pub name: String,
    pub transport: String,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub url: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Extra HTTP headers sent with every request to a `sse`/`streamable_http`
    /// server — e.g. an `Authorization` bearer token for a server requiring
    /// auth (never used for `stdio`).
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub status: String,
    pub error_message: Option<String>,
}

fn default_true() -> bool {
    true
}

/// What the frontend (or a builtin upsert) submits to create a server.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerSpec {
    pub name: String,
    pub transport: String,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub url: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// All-optional patch for `PATCH /api/mcp/servers/{id}` — only fields the
/// caller actually sets are sent, so an untouched field keeps its current
/// value server-side.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct McpServerPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

fn parse_server(row: &Value) -> Option<McpServer> {
    let args = row
        .get("args")
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
        .unwrap_or_default();
    let env = row
        .get("env")
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str::<HashMap<String, String>>(s).ok())
        .unwrap_or_default();
    let headers = row
        .get("headers")
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str::<HashMap<String, String>>(s).ok())
        .unwrap_or_default();
    Some(McpServer {
        id: row.get("id")?.as_str()?.to_string(),
        name: row.get("name")?.as_str()?.to_string(),
        transport: row.get("transport")?.as_str()?.to_string(),
        command: row.get("command").and_then(|v| v.as_str()).map(String::from),
        args,
        url: row.get("url").and_then(|v| v.as_str()).map(String::from),
        env,
        headers,
        enabled: row
            .get("enabled")
            .and_then(|v| v.as_i64())
            .map(|n| n != 0)
            .unwrap_or(true),
        status: row
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("disconnected")
            .to_string(),
        error_message: row
            .get("error_message")
            .and_then(|v| v.as_str())
            .map(String::from),
    })
}

pub async fn list_servers(client: &BigTinyClient) -> Result<Vec<McpServer>, String> {
    let resp = client.get_json("/api/mcp/servers").await?;
    Ok(resp
        .get("servers")
        .and_then(|v| v.as_array())
        .map(|rows| rows.iter().filter_map(parse_server).collect())
        .unwrap_or_default())
}

pub async fn create_server(client: &BigTinyClient, spec: &McpServerSpec) -> Result<String, String> {
    let body = json!({
        "name": spec.name,
        "transport": spec.transport,
        "command": spec.command,
        "args": spec.args,
        "url": spec.url,
        "env": spec.env,
        "headers": spec.headers,
        "enabled": spec.enabled,
    });
    let resp = client.post_json("/api/mcp/servers", &body).await?;
    resp.get("id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| "BigTiny did not return a server id".to_string())
}

pub async fn update_server(
    client: &BigTinyClient,
    id: &str,
    patch: &McpServerPatch,
) -> Result<McpServer, String> {
    let body = serde_json::to_value(patch).map_err(|e| e.to_string())?;
    let resp = client
        .patch_json(&format!("/api/mcp/servers/{id}"), &body)
        .await?;
    parse_server(&resp).ok_or_else(|| "BigTiny returned an unparseable MCP server".to_string())
}

pub async fn delete_server(client: &BigTinyClient, id: &str) -> Result<(), String> {
    client.delete(&format!("/api/mcp/servers/{id}")).await?;
    Ok(())
}

pub async fn connect_server(client: &BigTinyClient, id: &str) -> Result<(), String> {
    client
        .post_json(&format!("/api/mcp/servers/{id}/connect"), &json!({}))
        .await?;
    Ok(())
}

/// Idempotently (re)register Kitty's two bundled plugins as BigTiny MCP
/// servers, keyed by name so re-running never creates duplicates. Self-heals
/// the `command` path across an app update/reinstall and keeps `enabled` in
/// sync with the user's Settings toggle — the BigTiny-side replacement for
/// the old goosed-path's `replacement_mcp::ensure_registered` +
/// `lifecycle::start_stack`'s adaptive-pathway `config.yaml` env migration.
/// Best-effort throughout: failures are logged, never surfaced as errors.
pub async fn ensure_builtin_servers(app: &AppHandle) {
    let Ok(client) = ensure_client(app) else {
        return;
    };

    let (
        replacement_enabled,
        wasm_math_enabled,
        brave_search_enabled,
        ap_enabled,
        ap_db_path,
        ap_embedding_model,
        ollama_base,
    ) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        (
            cfg.replacement_mcp_enabled,
            cfg.wasm_math_mcp_enabled,
            cfg.brave_mcp_search_enabled,
            cfg.adaptive_pathway_enabled,
            cfg.adaptive_pathway_db_path.clone(),
            cfg.adaptive_pathway_embedding_model.clone(),
            cfg.ollama_base_url.clone(),
        )
    };

    let replacement_cmd = crate::config::bundled_plugin_path("replacement-mcp.exe")
        .unwrap_or_else(|| "replacement-mcp".to_string());
    upsert_builtin(
        &client,
        "replacement-mcp",
        &McpServerSpec {
            name: "replacement-mcp".to_string(),
            transport: "stdio".to_string(),
            command: Some(replacement_cmd),
            args: vec![],
            url: None,
            env: HashMap::new(),
            headers: HashMap::new(),
            enabled: replacement_enabled,
        },
    )
    .await;

    let ap_cmd = crate::config::bundled_plugin_path("adaptive-pathway-mcp.exe")
        .unwrap_or_else(|| "adaptive-pathway-mcp".to_string());
    let mut ap_env = HashMap::new();
    ap_env.insert("AP_EMBED_OLLAMA_MODEL".to_string(), ap_embedding_model);
    ap_env.insert("AP_EMBED_OLLAMA_URL".to_string(), ollama_base);
    upsert_builtin(
        &client,
        "adaptive-pathway",
        &McpServerSpec {
            name: "adaptive-pathway".to_string(),
            transport: "stdio".to_string(),
            command: Some(ap_cmd),
            args: vec!["--db-path".to_string(), ap_db_path],
            url: None,
            env: ap_env,
            headers: HashMap::new(),
            enabled: ap_enabled,
        },
    )
    .await;

    let wasm_math_cmd = crate::config::bundled_plugin_path("wasm-math-mcp.exe")
        .unwrap_or_else(|| "wasm-math-mcp".to_string());
    upsert_builtin(
        &client,
        "wasm-math-mcp",
        &McpServerSpec {
            name: "wasm-math-mcp".to_string(),
            transport: "stdio".to_string(),
            command: Some(wasm_math_cmd),
            args: vec![],
            url: None,
            env: HashMap::new(),
            headers: HashMap::new(),
            enabled: wasm_math_enabled,
        },
    )
    .await;

    // The API key is never read from config (see `brave_mcp_search_enabled`'s
    // doc comment) — only from the keyring, under a fixed id shared with
    // `commands::set_brave_mcp_search_api_key`/`set_brave_mcp_search_enabled`.
    // `brave_search_enabled` alone isn't enough to actually enable the server:
    // if the key was ever cleared (disable, or a keyring wipe) while the flag
    // was somehow left true, registering it "enabled" with an empty
    // BRAVE_API_KEY would just make every search call fail instead of
    // prompting the user to reconfigure — so gate on key presence too.
    let brave_api_key = crate::config::providers::get_secret_async("brave-mcp-search")
        .await
        .unwrap_or_default();
    let brave_cmd = crate::config::bundled_plugin_path("brave-mcp-search.exe")
        .unwrap_or_else(|| "brave-mcp-search".to_string());
    let mut brave_env = HashMap::new();
    if !brave_api_key.is_empty() {
        brave_env.insert("BRAVE_API_KEY".to_string(), brave_api_key.clone());
    }
    upsert_builtin(
        &client,
        "brave-mcp-search",
        &McpServerSpec {
            name: "brave-mcp-search".to_string(),
            transport: "stdio".to_string(),
            command: Some(brave_cmd),
            args: vec![],
            url: None,
            env: brave_env,
            headers: HashMap::new(),
            enabled: brave_search_enabled && !brave_api_key.is_empty(),
        },
    )
    .await;
}

async fn upsert_builtin(client: &BigTinyClient, name: &str, desired: &McpServerSpec) {
    let existing = match list_servers(client).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("bigtiny mcp list failed while syncing {name}: {e}");
            return;
        }
    };

    match existing.into_iter().find(|s| s.name == name) {
        None => match create_server(client, desired).await {
            Ok(id) => {
                if desired.enabled {
                    if let Err(e) = connect_server(client, &id).await {
                        tracing::warn!("bigtiny mcp connect failed for {name}: {e}");
                    }
                }
            }
            Err(e) => tracing::warn!("bigtiny mcp create failed for {name}: {e}"),
        },
        Some(row) => {
            let changed = row.command.as_deref() != desired.command.as_deref()
                || row.args != desired.args
                || row.env != desired.env
                || row.enabled != desired.enabled;
            if changed {
                let patch = McpServerPatch {
                    command: desired.command.clone(),
                    args: Some(desired.args.clone()),
                    env: Some(desired.env.clone()),
                    enabled: Some(desired.enabled),
                    ..Default::default()
                };
                if let Err(e) = update_server(client, &row.id, &patch).await {
                    tracing::warn!("bigtiny mcp update failed for {name}: {e}");
                }
            }
        }
    }
}
