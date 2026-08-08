//! Registry of MCP servers compiled into this daemon binary, reachable only
//! via `TransportType::InProcess` (see that variant's doc comment).
//!
//! This exists for hosts that can't spawn a bundled tool-server binary as a
//! child process — Android forbids `exec()` of anything in an app-writable
//! directory, which rules out `kitty-tools`' stdio subprocess the way
//! desktop Kitty runs it (`src-tauri/src/bigtiny/mcp.rs::ensure_builtin_servers`
//! upserts a `stdio` row pointing at the bundled exe). An in-process host
//! instead upserts an `in_process` row whose `command` field is one of the
//! names below, and `mcp::manager::connect_server` routes it here instead of
//! through `MCPServerClient::connect`.
//!
//! Adding a new in-process server (`kitty-web`, `kitty-wasm`, ...) means
//! adding it as a dependency here and a new arm in `connect` — nothing in
//! `mcp::manager`, `mcp::client`, or the DB schema needs to change.

use crate::error::MCPServerError;

use super::client::MCPServerClient;

/// Connects `name`'s in-process server under `server_id`, or an error if
/// `name` isn't a registered built-in — the in-process equivalent of a
/// stdio config pointing at a binary that doesn't exist on disk.
///
/// Note: adaptive-pathway is *not* wired here — its recall/record/learn paths
/// run in-process directly against the `PathwayEngine` (see
/// `agent::loop_`'s `pathway_recall`/turn-end pass), and the desktop host
/// registers `adaptive-pathway` as a stdio external binary instead. There is
/// deliberately no in-process `pathway` MCP server.
pub async fn connect(name: &str, server_id: String) -> Result<MCPServerClient, MCPServerError> {
    match name {
        "kitty-tools" => {
            MCPServerClient::connect_in_process(server_id, |stream| async move {
                if let Err(e) = kitty_tools::serve_in_process(stream).await {
                    tracing::error!("kitty-tools in-process server exited with error: {e}");
                }
            })
            .await
        }
        "kitty-web" => {
            MCPServerClient::connect_in_process(server_id, |stream| async move {
                if let Err(e) = kitty_web::serve_in_process(stream).await {
                    tracing::error!("kitty-web in-process server exited with error: {e}");
                }
            })
            .await
        }
        "kitty-wasm" => {
            MCPServerClient::connect_in_process(server_id, |stream| async move {
                if let Err(e) = kitty_wasm::serve_in_process(stream).await {
                    tracing::error!("kitty-wasm in-process server exited with error: {e}");
                }
            })
            .await
        }
        other => Err(MCPServerError::Generic(format!(
            "unknown in-process server: {other}"
        ))),
    }
}

/// Every registered built-in name, for callers that want to validate a
/// configured name before attempting a connect.
pub const BUILTIN_SERVERS: [&str; 3] = ["kitty-tools", "kitty-web", "kitty-wasm"];

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connects_the_kitty_tools_builtin_and_lists_its_tools() {
        let client = connect("kitty-tools", "test-kitty-tools".to_string())
            .await
            .expect("kitty-tools in-process connect should succeed");

        assert_eq!(client.server_id(), "test-kitty-tools");
        // Spot-check a couple of always-on tools rather than pinning the
        // full 17/18-tool surface here (that's `kitty-tools`' own
        // `tests/protocol.rs::tool_surface_matches_env_gating`'s job) — this
        // test exists to prove the in-process wiring round-trips a real
        // `tools/list`, not to duplicate that pin.
        let names: Vec<&str> = client.tools().iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"lean_file_read"));
        assert!(names.contains(&"lean_scratchpad_list"));
    }

    #[tokio::test]
    async fn unknown_builtin_name_is_a_clean_error_not_a_panic() {
        let result = connect("no-such-server", "test".to_string()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn connects_the_kitty_web_builtin_and_lists_its_tools() {
        let client = connect("kitty-web", "test-kitty-web".to_string())
            .await
            .expect("kitty-web in-process connect should succeed");
        let names: Vec<&str> = client.tools().iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"lean_web_search"));
        assert!(names.contains(&"lean_web_scrape"));
    }

    #[tokio::test]
    async fn connects_the_kitty_wasm_builtin_and_lists_its_tools() {
        let client = connect("kitty-wasm", "test-kitty-wasm".to_string())
            .await
            .expect("kitty-wasm in-process connect should succeed");
        let names: Vec<&str> = client.tools().iter().map(|t| t.name.as_str()).collect();
        // The name adaptive-pathway's learned routing is keyed on.
        assert!(names.contains(&"execute_math_python"));
        assert!(names.contains(&"wasm_run_module"));
    }

    #[tokio::test]
    async fn every_advertised_builtin_actually_connects() {
        // Guards against `BUILTIN_SERVERS` and `connect`'s match arms
        // drifting apart — a name listed but not wired would otherwise only
        // fail at runtime, on whatever host happened to configure it.
        for name in BUILTIN_SERVERS {
            connect(name, format!("test-{name}"))
                .await
                .unwrap_or_else(|e| panic!("advertised builtin {name} failed to connect: {e}"));
        }
    }
}
