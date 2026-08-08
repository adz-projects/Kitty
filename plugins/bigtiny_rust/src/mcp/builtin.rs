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

use std::sync::Arc;

use crate::error::MCPServerError;

use super::client::MCPServerClient;

/// Connects `name`'s in-process server under `server_id`, or an error if
/// `name` isn't a registered built-in — the in-process equivalent of a
/// stdio config pointing at a binary that doesn't exist on disk.
///
/// `pathway` is the odd one out: its recall path runs in-process directly
/// against the `PathwayEngine` (see `agent::loop_`'s `pathway_recall`), and
/// only its two *write* tools (`record`/`forget`) go through MCP — those are
/// choices the model makes, so they have to be callable as tools. It
/// therefore needs the engine handle `engine` carries, and returns a clean
/// error when pathway is configured off (`engine == None`) rather than
/// pretending to connect.
///
/// This used to have no `pathway` arm at all while
/// `src-tauri/src/bigtiny/mcp.rs::ensure_builtin_servers` upserted an
/// `in_process` row named `"pathway"` — the two sides disagreed, the row
/// resolved to `unknown in-process server: pathway`, and the model had no way
/// to correct a belief it knew was wrong. Do not re-remove it without also
/// removing that registration.
pub async fn connect(
    name: &str,
    server_id: String,
    engine: Option<Arc<adaptive_pathway::engine::PathwayEngine>>,
) -> Result<MCPServerClient, MCPServerError> {
    match name {
        "pathway" => {
            let engine = engine.ok_or_else(|| {
                MCPServerError::Generic(
                    "pathway MCP server requested but the behavioral-memory engine is disabled"
                        .to_string(),
                )
            })?;
            MCPServerClient::connect_in_process(server_id, |stream| async move {
                // Empty session id: this connection is daemon-lifetime and
                // shared across concurrently-streaming sessions, so the
                // per-call session is injected into the tool arguments by
                // `agent::loop_`'s dispatch site instead. See
                // `PathwayServer::session_scope`.
                let server = adaptive_pathway::mcp::PathwayServer::new(engine, String::new());
                if let Err(e) = server.serve_in_process(stream).await {
                    tracing::error!("pathway in-process server exited with error: {e}");
                }
            })
            .await
        }
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
pub const BUILTIN_SERVERS: [&str; 4] = ["kitty-tools", "kitty-web", "kitty-wasm", "pathway"];

/// Tool names owned by the `pathway` server, which need the executing
/// session id injected into their arguments before dispatch
/// (`agent::loop_::run_single_tool`). Kept here next to the server that
/// provides them so a renamed tool can't silently stop being injected.
pub const PATHWAY_TOOLS: [&str; 2] = ["record", "forget"];

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connects_the_kitty_tools_builtin_and_lists_its_tools() {
        let client = connect("kitty-tools", "test-kitty-tools".to_string(), None)
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
        let result = connect("no-such-server", "test".to_string(), None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn connects_the_kitty_web_builtin_and_lists_its_tools() {
        let client = connect("kitty-web", "test-kitty-web".to_string(), None)
            .await
            .expect("kitty-web in-process connect should succeed");
        let names: Vec<&str> = client.tools().iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"lean_web_search"));
        assert!(names.contains(&"lean_web_scrape"));
    }

    #[tokio::test]
    async fn connects_the_kitty_wasm_builtin_and_lists_its_tools() {
        let client = connect("kitty-wasm", "test-kitty-wasm".to_string(), None)
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
        // fail at runtime, on whatever host happened to configure it. This
        // is exactly how `pathway` stayed broken: `src-tauri` registered an
        // `in_process` row for it while `connect` had no arm.
        let engine = adaptive_pathway::engine::PathwayEngine::open_in_memory(
            adaptive_pathway::config::Config::default(),
        )
        .await
        .expect("in-memory pathway engine");
        for name in BUILTIN_SERVERS {
            connect(name, format!("test-{name}"), Some(engine.clone()))
                .await
                .unwrap_or_else(|e| panic!("advertised builtin {name} failed to connect: {e}"));
        }
    }

    #[tokio::test]
    async fn the_pathway_server_advertises_record_and_forget() {
        let engine = adaptive_pathway::engine::PathwayEngine::open_in_memory(
            adaptive_pathway::config::Config::default(),
        )
        .await
        .expect("in-memory pathway engine");
        let client = connect("pathway", "test-pathway".to_string(), Some(engine))
            .await
            .expect("pathway in-process connect should succeed");
        let names: Vec<&str> = client.tools().iter().map(|t| t.name.as_str()).collect();
        for tool in PATHWAY_TOOLS {
            assert!(
                names.contains(&tool),
                "pathway must advertise {tool}; it's what lets the model correct its own memory"
            );
        }
    }

    #[tokio::test]
    async fn pathway_fails_cleanly_when_the_engine_is_disabled() {
        // `engine == None` whenever behavioral memory is configured off. A
        // clean error is right; half-connecting a server whose tools would
        // then panic is not.
        match connect("pathway", "test-pathway-off".to_string(), None).await {
            Ok(_) => panic!("pathway must not connect without an engine"),
            Err(e) => assert!(e.to_string().contains("disabled"), "unexpected error: {e}"),
        }
    }
}
