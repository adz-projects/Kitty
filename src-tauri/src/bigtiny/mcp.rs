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
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
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
        command: row
            .get("command")
            .and_then(|v| v.as_str())
            .map(String::from),
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
/// Builtins retired by the Rust consolidation — their tools now live inside
/// the single `kitty-tools` server (see the block below). `upsert_builtin`
/// only ever creates/patches/connects a row matching its own desired name;
/// it never calls `delete_server` for anything, so without this pass these
/// three would persist forever in BigTiny's DB pointing at exes that no
/// longer ship, each surfacing in Settings as a permanently-failing
/// user-added card (since `HIDDEN_SERVER_NAMES` only hides the *current*
/// name set). Called before any upsert so a stale row never races a fresh
/// create under the same name.
const RETIRED_BUILTINS: &[&str] = &["replacement-mcp", "brave-mcp-search", "visualizations"];

async fn remove_retired_builtins(client: &BigTinyClient) {
    let Some(existing) = list_servers_with_retry(client, "retired-builtins-cleanup").await else {
        return;
    };
    for name in RETIRED_BUILTINS {
        if let Some(row) = existing.iter().find(|s| s.name == *name) {
            if let Err(e) = delete_server(client, &row.id).await {
                tracing::warn!("failed to remove retired builtin {name}: {e}");
            }
        }
    }
}

pub async fn ensure_builtin_servers(app: &AppHandle) {
    let Ok(client) = ensure_client(app) else {
        return;
    };

    remove_retired_builtins(&client).await;

    let (
        wasm_math_enabled,
        brave_search_enabled,
        visualizations_enabled,
        kitty_tools_enabled,
        kitty_docs_web_enabled,
        ap_enabled,
        ap_db_path,
        ap_embedding_model,
        ollama_base,
        ap_port,
    ) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        (
            cfg.wasm_math_mcp_enabled,
            cfg.brave_mcp_search_enabled,
            cfg.visualizations_enabled,
            cfg.kitty_tools_enabled,
            cfg.kitty_docs_web_enabled,
            cfg.adaptive_pathway_enabled,
            cfg.adaptive_pathway_db_path.clone(),
            cfg.adaptive_pathway_embedding_model.clone(),
            cfg.ollama_base_url.clone(),
            cfg.adaptive_pathway_port,
        )
    };

    let ap_cmd = crate::config::bundled_plugin_path("adaptive-pathway-mcp.exe")
        .unwrap_or_else(|| "adaptive-pathway-mcp".to_string());
    let mut ap_env = HashMap::new();
    ap_env.insert("AP_EMBED_OLLAMA_MODEL".to_string(), ap_embedding_model);
    ap_env.insert("AP_EMBED_OLLAMA_URL".to_string(), ollama_base);
    // The MCP server is a stateless HTTP proxy to the sidecar; it must know
    // which port the sidecar (spawned separately by `lifecycle`) is bound to.
    ap_env.insert("AP_SIDECAR_PORT".to_string(), ap_port.to_string());
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

    // `kitty-tools` hosts the local-machine tool set — 18 always-on
    // `lean_*` tools (shell/workspace/file/word/cache/scratchpad), plus the
    // 3 visualization tools gated by `KITTY_VIZ_ENABLED` rather than
    // registered as their own separate server. `enabled` alone
    // (`kitty_tools_enabled`) controls whether the whole server is
    // registered at all; `visualizations_enabled` only controls which tools
    // it advertises once running — matching the "remove tools from the
    // router at startup rather than registering them and failing at call
    // time" design in `plugins/kitty-tools/src/server.rs`. Web search does
    // NOT live here — see the `kitty-docs-web` block below for why.
    let kitty_tools_cmd = crate::config::bundled_plugin_path("kitty-tools.exe")
        .unwrap_or_else(|| "kitty-tools".to_string());
    let mut kitty_tools_env = HashMap::new();
    if visualizations_enabled {
        kitty_tools_env.insert("KITTY_VIZ_ENABLED".to_string(), "1".to_string());
    }
    upsert_builtin(
        &client,
        "kitty-tools",
        &McpServerSpec {
            name: "kitty-tools".to_string(),
            transport: "stdio".to_string(),
            command: Some(kitty_tools_cmd),
            args: vec![],
            url: None,
            env: kitty_tools_env,
            headers: HashMap::new(),
            enabled: kitty_tools_enabled,
        },
    )
    .await;

    // PDF/Excel/web-scrape/web-search — the other half of the
    // `replacement-mcp` split (see `plugins/kitty-docs-web/`). Hosts the
    // merged, count-tiered `lean_web_search`/`lean_web_search_read_chunk`:
    // Brave preferred (with automatic DuckDuckGo fallback) for small
    // requests, both engines queried together for broader ones, and an
    // offloaded/indexed mode for large ones. This tool used to live in
    // `kitty-tools` (Rust) as `brave_mcp_search`; it moved here because
    // DuckDuckGo's `ddgs` library has no Rust crate equivalent, while
    // Brave's call is a plain JSON GET, trivial to host alongside `ddgs`
    // instead — see `docs/VERSIONS.md`. `BRAVE_API_KEY` now attaches to
    // *this* server's env map rather than `kitty-tools`'s.
    let kitty_docs_web_cmd = crate::config::bundled_plugin_path("kitty-docs-web.exe")
        .unwrap_or_else(|| "kitty-docs-web".to_string());
    let mut kitty_docs_web_env = HashMap::new();

    // The API key is never read from config (see `brave_mcp_search_enabled`'s
    // doc comment) — only from the keyring, under a fixed id shared with
    // `commands::set_brave_mcp_search_api_key`/`set_brave_mcp_search_enabled`.
    //
    // Use the checked read, not `get_secret_async`: a *transient* Credential
    // Manager read failure must not be treated the same as "no key stored" —
    // that used to silently disable the server (and desync it from the
    // Settings checkbox, which reads via a separate, later, synchronous
    // `has_secret` call) on nothing more than momentary OS contention. Only a
    // confirmed absence of the entry omits `BRAVE_API_KEY`; a read error
    // skips the *entire* kitty-docs-web sync this pass (not just Brave)
    // rather than upserting a spec with the key silently dropped, which would
    // PATCH `env` and disable Brave preference for everyone on nothing more
    // than momentary keyring contention.
    match crate::config::providers::get_secret_checked("brave-mcp-search").await {
        Ok(brave_api_key) => {
            let brave_api_key = brave_api_key.unwrap_or_default();
            if brave_search_enabled && !brave_api_key.is_empty() {
                kitty_docs_web_env.insert("BRAVE_API_KEY".to_string(), brave_api_key);
            }
            upsert_builtin(
                &client,
                "kitty-docs-web",
                &McpServerSpec {
                    name: "kitty-docs-web".to_string(),
                    transport: "stdio".to_string(),
                    command: Some(kitty_docs_web_cmd),
                    args: vec![],
                    url: None,
                    env: kitty_docs_web_env,
                    headers: HashMap::new(),
                    enabled: kitty_docs_web_enabled,
                },
            )
            .await;
        }
        Err(e) => {
            tracing::warn!(
                "brave-mcp-search keyring read failed ({e}); skipping kitty-docs-web sync this pass to avoid disabling Brave preference on a transient error"
            );
        }
    }
}

/// Periodic self-heal re-sync of the bundled MCP servers, run from the
/// Adaptive Pathway health loop (on the same ~30s cadence as the embedding
/// model recheck) — not just once at startup. Re-uses `ensure_builtin_servers`,
/// whose `decide_sync_action` issues a `Connect` for any enabled row that
/// isn't `connected` (a transient connect failure on boot, or a sidecar-port
/// change after startup), so a dropped builtin server recovers by itself
/// instead of staying dead until the app restarts. Best-effort: logs only.
pub async fn self_heal_builtin_servers(app: &AppHandle) {
    ensure_builtin_servers(app).await;
}

/// Number of attempts (with a short backoff between) for the `list_servers`
/// call that gates every builtin sync. A single failure here used to give up
/// on the *entire* sync for the rest of the session (see `spawn`'s discarded
/// health-probe result) — retrying a few times covers the case where the
/// daemon has just barely finished binding.
const LIST_SERVERS_ATTEMPTS: u32 = 3;
const LIST_SERVERS_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

async fn list_servers_with_retry(client: &BigTinyClient, name: &str) -> Option<Vec<McpServer>> {
    for attempt in 1..=LIST_SERVERS_ATTEMPTS {
        match list_servers(client).await {
            Ok(rows) => return Some(rows),
            Err(e) if attempt < LIST_SERVERS_ATTEMPTS => {
                tracing::warn!(
                    "bigtiny mcp list failed while syncing {name} (attempt {attempt}/{LIST_SERVERS_ATTEMPTS}): {e}"
                );
                tokio::time::sleep(LIST_SERVERS_RETRY_DELAY).await;
            }
            Err(e) => {
                tracing::warn!(
                    "bigtiny mcp list failed while syncing {name}, giving up after {LIST_SERVERS_ATTEMPTS} attempts: {e}"
                );
            }
        }
    }
    None
}

/// What `upsert_builtin` should do, given the current BigTiny-side rows and
/// the desired spec. Split out as a pure function so the decision — in
/// particular "an already-matching row that never connected must still be
/// reconnected" — is unit-testable without a live daemon.
#[derive(Debug, Clone, PartialEq)]
enum SyncAction {
    Create,
    Patch {
        row_id: String,
        patch: Box<McpServerPatch>,
    },
    Connect {
        row_id: String,
    },
    Noop,
}

fn decide_sync_action(existing: &[McpServer], name: &str, desired: &McpServerSpec) -> SyncAction {
    let Some(row) = existing.iter().find(|s| s.name == name) else {
        return SyncAction::Create;
    };

    let changed = row.command.as_deref() != desired.command.as_deref()
        || row.args != desired.args
        || row.env != desired.env
        || row.enabled != desired.enabled;

    if changed {
        SyncAction::Patch {
            row_id: row.id.clone(),
            patch: Box::new(McpServerPatch {
                command: desired.command.clone(),
                args: Some(desired.args.clone()),
                env: Some(desired.env.clone()),
                enabled: Some(desired.enabled),
                ..Default::default()
            }),
        }
    } else if desired.enabled && row.status != "connected" {
        // The row already matches what we want, but it never actually
        // connected (e.g. a slow onefile self-extraction blew BigTiny's own
        // connect timeout during startup, or a previous best-effort
        // reconnect failed after an `env` change and was swallowed
        // server-side). A PATCH with no field changes wouldn't trigger
        // BigTiny to retry, so ask it to connect directly instead of leaving
        // the server dead for the rest of the session.
        SyncAction::Connect {
            row_id: row.id.clone(),
        }
    } else {
        SyncAction::Noop
    }
}

/// Cheap round-trip against the same Brave endpoint `lean_web_search` itself
/// calls, used only to confirm a freshly-entered API key actually works
/// before storing it. Without this, a wrong/revoked/mistyped key still ends
/// up `configured: true` with a green Settings checkbox, and every
/// subsequent search silently comes back `AUTH_ERROR` — a confusing,
/// hard-to-diagnose dead end for something that could be caught immediately.
///
/// Fails open (`Ok`) on anything other than a confirmed 401/403: a transient
/// network hiccup during this one validation call must not block saving a
/// key that may well be fine.
pub async fn validate_brave_api_key(api_key: &str) -> Result<(), String> {
    let client = crate::util::http_client();
    let resp = client
        .get("https://api.search.brave.com/res/v1/llm/context")
        .query(&[("q", "kitty-mcp-key-validation"), ("count", "1")])
        .header("Accept", "application/json")
        .header("Accept-Encoding", "gzip")
        .header("X-Subscription-Token", api_key)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;

    match resp {
        Ok(r)
            if r.status() == reqwest::StatusCode::UNAUTHORIZED
                || r.status() == reqwest::StatusCode::FORBIDDEN =>
        {
            Err("Brave rejected this API key (unauthorized) — check that it was copied correctly and is still active.".to_string())
        }
        _ => Ok(()),
    }
}

async fn upsert_builtin(client: &BigTinyClient, name: &str, desired: &McpServerSpec) {
    let Some(existing) = list_servers_with_retry(client, name).await else {
        return;
    };

    match decide_sync_action(&existing, name, desired) {
        SyncAction::Create => match create_server(client, desired).await {
            Ok(id) => {
                if desired.enabled {
                    if let Err(e) = connect_server(client, &id).await {
                        tracing::warn!("bigtiny mcp connect failed for {name}: {e}");
                    }
                }
            }
            Err(e) => tracing::warn!("bigtiny mcp create failed for {name}: {e}"),
        },
        SyncAction::Patch { row_id, patch } => {
            if let Err(e) = update_server(client, &row_id, &patch).await {
                tracing::warn!("bigtiny mcp update failed for {name}: {e}");
            }
        }
        SyncAction::Connect { row_id } => {
            if let Err(e) = connect_server(client, &row_id).await {
                tracing::warn!("bigtiny mcp reconnect failed for {name}: {e}");
            }
        }
        SyncAction::Noop => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, command: &str, enabled: bool) -> McpServerSpec {
        McpServerSpec {
            name: name.to_string(),
            transport: "stdio".to_string(),
            command: Some(command.to_string()),
            args: vec![],
            url: None,
            env: HashMap::new(),
            headers: HashMap::new(),
            enabled,
        }
    }

    fn row(name: &str, command: &str, enabled: bool, status: &str) -> McpServer {
        McpServer {
            id: format!("{name}-id"),
            name: name.to_string(),
            transport: "stdio".to_string(),
            command: Some(command.to_string()),
            args: vec![],
            url: None,
            env: HashMap::new(),
            headers: HashMap::new(),
            enabled,
            status: status.to_string(),
            error_message: None,
        }
    }

    #[test]
    fn creates_when_no_row_exists() {
        let desired = spec("brave-mcp-search", "brave-mcp-search.exe", true);
        assert_eq!(
            decide_sync_action(&[], "brave-mcp-search", &desired),
            SyncAction::Create
        );
    }

    #[test]
    fn patches_when_a_field_differs() {
        let existing = vec![row("brave-mcp-search", "old-path.exe", true, "connected")];
        let desired = spec("brave-mcp-search", "new-path.exe", true);
        match decide_sync_action(&existing, "brave-mcp-search", &desired) {
            SyncAction::Patch { row_id, .. } => assert_eq!(row_id, "brave-mcp-search-id"),
            other => panic!("expected Patch, got {other:?}"),
        }
    }

    /// The bug this addendum fixes: a row that already matches `desired` in
    /// every field but never connected (BigTiny's own connect attempt failed
    /// or timed out) must still trigger a connect — a plain equality check
    /// would call this a no-op forever.
    #[test]
    fn reconnects_a_matching_but_unconnected_row_when_desired_is_enabled() {
        let existing = vec![row(
            "brave-mcp-search",
            "brave-mcp-search.exe",
            true,
            "error",
        )];
        let desired = spec("brave-mcp-search", "brave-mcp-search.exe", true);
        match decide_sync_action(&existing, "brave-mcp-search", &desired) {
            SyncAction::Connect { row_id } => assert_eq!(row_id, "brave-mcp-search-id"),
            other => panic!("expected Connect, got {other:?}"),
        }
    }

    #[test]
    fn does_not_reconnect_a_disconnected_row_when_desired_is_disabled() {
        let existing = vec![row(
            "brave-mcp-search",
            "brave-mcp-search.exe",
            false,
            "disconnected",
        )];
        let desired = spec("brave-mcp-search", "brave-mcp-search.exe", false);
        assert_eq!(
            decide_sync_action(&existing, "brave-mcp-search", &desired),
            SyncAction::Noop
        );
    }

    #[test]
    fn is_a_noop_when_everything_already_matches_and_is_connected() {
        let existing = vec![row(
            "brave-mcp-search",
            "brave-mcp-search.exe",
            true,
            "connected",
        )];
        let desired = spec("brave-mcp-search", "brave-mcp-search.exe", true);
        assert_eq!(
            decide_sync_action(&existing, "brave-mcp-search", &desired),
            SyncAction::Noop
        );
    }
}
