use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::error::MCPServerError;
use crate::models::mcp::{MCPServerConfig, ToolDefinition, ToolResult, TransportType};
use crate::storage::mcp_servers;

use super::client::MCPServerClient;
use super::tools::validate_tool_args;

const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// Registry/dispatcher over all connected MCP servers. Ports
/// `plugins/bigtiny/bigtiny/mcp/manager.py::MCPManager`.
///
/// `tool_registry` is a deliberately *flat* namespace across all servers —
/// later-connected servers silently shadow same-named tools from earlier
/// ones, matching the Python reference exactly. Do not "fix" this into a
/// namespaced design; existing deployments depend on tool names being
/// effectively global.
pub struct MCPManager {
    pool: SqlitePool,
    /// `Arc` so a tool call can clone a client handle out of the map, drop the
    /// DashMap guard, and only then `.await` the call — holding the shard
    /// lock across an await previously blocked every sibling tool call on the
    /// same shard for the whole call duration.
    servers: DashMap<String, Arc<MCPServerClient>>,
    tool_registry: DashMap<String, ToolDefinition>,
}

impl MCPManager {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            servers: DashMap::new(),
            tool_registry: DashMap::new(),
        }
    }

    pub async fn connect_server(&self, server_id: &str) -> Result<(), MCPServerError> {
        let row = mcp_servers::get_server(&self.pool, server_id)
            .await
            .map_err(|e| MCPServerError::Generic(e.to_string()))?
            .ok_or_else(|| MCPServerError::NotFound(server_id.to_string()))?;

        let config = row_to_config(&row);

        // `InProcess` has no command/url `MCPServerClient::connect` could
        // dial — `command` here is a logical name looked up in
        // `mcp::builtin`'s registry instead. Both branches still share the
        // same connect-timeout/status-update/evict-stale handling below, so
        // an in-process server gets identical enable-toggle and
        // failed-reconnect behavior to a stdio one.
        let connect_result = if config.transport == TransportType::InProcess {
            let name = config.command.clone().unwrap_or_default();
            tokio::time::timeout(CONNECT_TIMEOUT, super::builtin::connect(&name, server_id.to_string()))
            .await
        } else {
            tokio::time::timeout(CONNECT_TIMEOUT, MCPServerClient::connect(&config)).await
        };

        match connect_result {
            Ok(Ok(client)) => {
                // Prune this server's previous (possibly stale) registry
                // entries before advertising the fresh tool list — on a
                // successful reconnect, tools the server no longer advertises
                // used to linger in the flat registry forever, keeping dead
                // tools callable (routing to the stale client) after a code
                // change or server-side tool removal.
                self.prune_registry_for(server_id);
                for tool in client.tools() {
                    self.tool_registry.insert(tool.name.clone(), tool.clone());
                }
                self.servers.insert(server_id.to_string(), Arc::new(client));
                let _ = mcp_servers::update_status(&self.pool, server_id, "connected", None).await;
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = mcp_servers::update_status(
                    &self.pool,
                    server_id,
                    "error",
                    Some(&e.to_string()),
                )
                .await;
                self.evict_stale(server_id).await;
                Err(e)
            }
            Err(_) => {
                let msg = format!("connect timed out after {}s", CONNECT_TIMEOUT.as_secs());
                let _ =
                    mcp_servers::update_status(&self.pool, server_id, "error", Some(&msg)).await;
                self.evict_stale(server_id).await;
                Err(MCPServerError::Timeout(CONNECT_TIMEOUT.as_secs_f64()))
            }
        }
    }

    /// Drop every registry entry advertising a tool for `server_id` — the
    /// flat registry is keyed by tool *name*, so a server's tools are
    /// identified by `ToolDefinition.server_id` rather than the key. Used on
    /// successful (re)connect (replace the old tool set before installing the
    /// fresh one), on failed (re)connect, and on disconnect.
    fn prune_registry_for(&self, server_id: &str) {
        self.tool_registry.retain(|_, t| t.server_id != server_id);
    }

    /// Remove a previous (now-stale) client + its tools for `server_id`, if
    /// any — called when a (re)connect attempt fails. Without this, a failed
    /// reconnect left the last-known-good client and its tool names in
    /// place: calls kept routing to a dead client (hanging until
    /// `DEFAULT_TOOL_TIMEOUT` instead of failing fast), and the tool
    /// registry kept advertising tools that were no longer reachable.
    async fn evict_stale(&self, server_id: &str) {
        if let Some((_, client)) = self.servers.remove(server_id) {
            self.prune_registry_for(server_id);
            // `Arc::try_unwrap` recovers ownership of the client (needed for
            // the consuming `shutdown`); a live clone means an in-flight call
            // still holds it, in which case the drop-on-last-strong-ref tears
            // the handle down anyway.
            if let Ok(client) = Arc::try_unwrap(client) {
                client.shutdown().await;
            }
        }
    }

    /// Connect every `enabled` server concurrently. Each server's failure is
    /// isolated (logged, not propagated) so one bad server can't fail startup.
    pub async fn connect_all(&self) {
        let rows = match mcp_servers::list_servers(&self.pool).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("failed to list mcp servers: {e}");
                return;
            }
        };
        let enabled: Vec<String> = rows
            .into_iter()
            .filter(|r| r.enabled != 0)
            .map(|r| r.id)
            .collect();

        let futures = enabled.into_iter().map(|id| async move {
            if let Err(e) = self.connect_server(&id).await {
                tracing::warn!("failed to connect mcp server {id}: {e}");
            }
        });
        futures::future::join_all(futures).await;
    }

    /// Sorted by tool name — feeds directly into the request-head tool-hints
    /// system message (`agent/context/builder.rs`), so a stable order here is
    /// required for LLM prompt-prefix caching to hit turn over turn. Without
    /// it, `tool_registry`'s `DashMap` iteration order (and, for a single
    /// server, whatever order that server's `tools/list` happened to return)
    /// is unspecified and can silently change between turns.
    pub fn list_tools(&self, server_id: Option<&str>) -> Vec<ToolDefinition> {
        let mut tools = match server_id {
            Some(id) => self
                .servers
                .get(id)
                .map(|c| c.tools().to_vec())
                .unwrap_or_default(),
            None => self
                .tool_registry
                .iter()
                .map(|e| e.value().clone())
                .collect(),
        };
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    /// Whether a tool name is currently registered (cheap `DashMap` lookup;
    /// used by the Adaptive Pathway turn hooks to gate on the AP MCP server
    /// being connected without a full `list_tools` round-trip per tool call).
    pub fn has_tool(&self, name: &str) -> bool {
        self.tool_registry.contains_key(name)
    }

    /// Never returns an `Err` — every failure mode (unknown tool, server not
    /// connected, invalid args, timeout, transport/protocol error) is encoded
    /// as `ToolResult { is_error: true, .. }`. Callers run tool calls
    /// concurrently and one failure must not cancel its siblings.
    pub async fn execute_tool(
        &self,
        tool_name: &str,
        args: &Value,
        timeout: Option<Duration>,
    ) -> ToolResult {
        let timeout = timeout.unwrap_or(DEFAULT_TOOL_TIMEOUT);

        let Some(tool) = self.tool_registry.get(tool_name).map(|e| e.value().clone()) else {
            return error_result(tool_name, format!("[Unknown tool: {tool_name}]"));
        };

        if let Err(msg) = validate_tool_args(&tool, args) {
            return error_result(
                tool_name,
                format!("[Invalid arguments for tool '{tool_name}': {msg}]"),
            );
        }

        // Clone the client handle (an `Arc`) out of the map and drop the
        // DashMap guard before awaiting — the call can run for up to
        // `DEFAULT_TOOL_TIMEOUT`, and holding the shard lock across it would
        // block every sibling tool call sharing that shard.
        let Some(client) = self.servers.get(&tool.server_id).map(|c| c.value().clone()) else {
            return error_result(
                tool_name,
                format!("[Server for tool '{tool_name}' is not connected]"),
            );
        };

        client.execute_tool(tool_name, args, timeout).await
    }

    pub async fn disconnect_server(&self, server_id: &str) {
        if let Some((_, client)) = self.servers.remove(server_id) {
            self.prune_registry_for(server_id);
            if let Ok(client) = Arc::try_unwrap(client) {
                client.shutdown().await;
            }
        }
        let _ = mcp_servers::update_status(&self.pool, server_id, "disconnected", None).await;
    }

    pub async fn disconnect_all(&self) {
        let ids: Vec<String> = self.servers.iter().map(|e| e.key().clone()).collect();
        for id in ids {
            self.disconnect_server(&id).await;
        }
    }
}

fn error_result(tool_name: &str, content: String) -> ToolResult {
    ToolResult {
        content,
        tool_call_id: format!(
            "{tool_name}_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ),
        duration_ms: 0,
        output_size_bytes: 0,
        is_error: true,
        truncated: false,
    }
}

fn row_to_config(row: &mcp_servers::MCPServerRow) -> MCPServerConfig {
    let transport = match row.transport.as_str() {
        "stdio" => TransportType::Stdio,
        "sse" => TransportType::Sse,
        "in_process" => TransportType::InProcess,
        _ => TransportType::StreamableHttp,
    };
    MCPServerConfig {
        id: row.id.clone(),
        name: row.name.clone(),
        transport,
        command: row.command.clone(),
        args: row.args.as_ref().and_then(|s| serde_json::from_str(s).ok()),
        url: row.url.clone(),
        env: row.env.as_ref().and_then(|s| serde_json::from_str(s).ok()),
        headers: row
            .headers
            .as_ref()
            .and_then(|s| serde_json::from_str(s).ok())
            .map(decrypt_headers_value),
        status: row.status.clone(),
        error_message: row.error_message.clone(),
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

/// Decrypt every value in a parsed `headers` object — the single chokepoint
/// feeding both `mcp/client.rs` and `mcp/sse_transport.rs`, so neither of
/// those needs to know encryption exists. `decrypt` itself transparently
/// passes through legacy-plaintext values, so this is safe to run
/// unconditionally on whatever's stored.
fn decrypt_headers_value(headers: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(obj) = headers else {
        return headers;
    };
    let decrypted: serde_json::Map<String, serde_json::Value> = obj
        .into_iter()
        .map(|(k, v)| {
            let v = match v.as_str() {
                Some(s) => serde_json::json!(crate::crypto::decrypt(s)),
                None => v,
            };
            (k, v)
        })
        .collect();
    serde_json::Value::Object(decrypted)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            server_id: "srv".to_string(),
        }
    }

    #[tokio::test]
    async fn list_tools_is_sorted_and_stable_across_calls() {
        let pool = test_pool().await;
        let manager = MCPManager::new(pool);
        // Insert in deliberately shuffled order.
        for name in ["zebra", "alpha", "mike"] {
            manager.tool_registry.insert(name.to_string(), tool(name));
        }

        let first: Vec<String> = manager
            .list_tools(None)
            .into_iter()
            .map(|t| t.name)
            .collect();
        let second: Vec<String> = manager
            .list_tools(None)
            .into_iter()
            .map(|t| t.name)
            .collect();

        assert_eq!(first, vec!["alpha", "mike", "zebra"]);
        assert_eq!(
            first, second,
            "tool order must be identical across repeat calls"
        );
    }

    /// Regression (WS3-4): a successful reconnect must prune the previous
    /// tool set for that server — tools the server no longer advertises
    /// otherwise linger in the flat registry forever.
    #[tokio::test]
    async fn prune_registry_for_drops_only_that_servers_tools() {
        let pool = test_pool().await;
        let manager = MCPManager::new(pool);
        for (name, server) in [
            ("a_tool", "srv-a"),
            ("a2_tool", "srv-a"),
            ("b_tool", "srv-b"),
        ] {
            manager.tool_registry.insert(
                name.to_string(),
                ToolDefinition {
                    name: name.to_string(),
                    description: String::new(),
                    input_schema: serde_json::json!({}),
                    server_id: server.to_string(),
                },
            );
        }

        manager.prune_registry_for("srv-a");

        let names: Vec<String> = manager
            .tool_registry
            .iter()
            .map(|e| e.key().clone())
            .collect();
        assert!(!names.contains(&"a_tool".to_string()));
        assert!(!names.contains(&"a2_tool".to_string()));
        assert!(names.contains(&"b_tool".to_string()));
    }
}
