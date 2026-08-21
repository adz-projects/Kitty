//! Verifies `MCPManager::execute_tool`'s never-throws contract (Phase B.4 of
//! BigTinyRust continuation plan) against a real stdio subprocess
//! (`tests/bin/fake_mcp_server.rs`): unknown tool, invalid args, a call that
//! times out, and a subprocess that crashes mid-call must all come back as
//! `ToolResult { is_error: true, .. }` — never a panic, never an `Err`
//! propagated out of the call (which would cancel sibling concurrent tool
//! calls via `join_all`).

use std::time::Duration;

use bigtiny_rust::mcp::MCPManager;
use serde_json::json;
use sqlx::SqlitePool;

async fn setup_pool_with_server() -> (SqlitePool, String) {
    setup_pool_with_args("[]").await
}

/// `args` is the JSON array stored in the `mcp_servers.args` column.
async fn setup_pool_with_args(args: &str) -> (SqlitePool, String) {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::query("PRAGMA journal_mode = WAL")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let server_id = "fake-server";
    let command = env!("CARGO_BIN_EXE_fake_mcp_server");
    sqlx::query(
        "INSERT INTO mcp_servers (id, name, transport, command, args, enabled, status) \
         VALUES (?, ?, 'stdio', ?, ?, 1, 'disconnected')",
    )
    .bind(server_id)
    .bind("fake")
    .bind(command)
    .bind(args)
    .execute(&pool)
    .await
    .unwrap();

    (pool, server_id.to_string())
}

#[tokio::test]
async fn execute_tool_success_roundtrip() {
    let (pool, server_id) = setup_pool_with_server().await;
    let manager = MCPManager::new(pool, None);
    manager
        .connect_server(&server_id)
        .await
        .expect("connect should succeed");

    let result = manager
        .execute_tool("echo_tool", &json!({"text": "hello"}), None)
        .await;
    assert!(!result.is_error, "unexpected error: {}", result.content);
    assert_eq!(result.content, "hello");
}

#[tokio::test]
async fn execute_tool_unknown_tool_never_errors() {
    let (pool, server_id) = setup_pool_with_server().await;
    let manager = MCPManager::new(pool, None);
    manager
        .connect_server(&server_id)
        .await
        .expect("connect should succeed");

    let result = manager
        .execute_tool("does_not_exist", &json!({}), None)
        .await;
    assert!(result.is_error);
    assert!(result.content.contains("Unknown tool"));
}

#[tokio::test]
async fn execute_tool_bad_args_never_errors() {
    let (pool, server_id) = setup_pool_with_server().await;
    let manager = MCPManager::new(pool, None);
    manager
        .connect_server(&server_id)
        .await
        .expect("connect should succeed");

    let result = manager.execute_tool("echo_tool", &json!({}), None).await;
    assert!(result.is_error);
    assert!(result.content.contains("Invalid arguments"));
}

#[tokio::test]
async fn execute_tool_timeout_never_errors() {
    let (pool, server_id) = setup_pool_with_server().await;
    let manager = MCPManager::new(pool, None);
    manager
        .connect_server(&server_id)
        .await
        .expect("connect should succeed");

    let result = manager
        .execute_tool(
            "sleep_tool",
            &json!({"millis": 5000}),
            Some(Duration::from_millis(200)),
        )
        .await;
    assert!(result.is_error);
    assert!(result.content.contains("timed out"));
}

#[tokio::test]
async fn execute_tool_crashed_subprocess_never_errors() {
    let (pool, server_id) = setup_pool_with_server().await;
    let manager = MCPManager::new(pool, None);
    manager
        .connect_server(&server_id)
        .await
        .expect("connect should succeed");

    let result = manager
        .execute_tool("crash_tool", &json!({}), Some(Duration::from_secs(5)))
        .await;
    assert!(result.is_error);
}

/// Regression for the "one bad line kills the server" failure: a third-party
/// stdio server that writes plain log lines to stdout (extremely common) used
/// to take its entire tool set offline at the first one, because rmcp's own
/// transport maps any decode error to end-of-stream. `mcp::rw_transport` skips
/// the junk and keeps the connection, so the handshake, tool listing and tool
/// calls must all still work with a server that is noisy on every message.
#[tokio::test]
async fn a_server_that_logs_plain_text_to_stdout_still_works() {
    let (pool, server_id) = setup_pool_with_args(r#"["--noisy"]"#).await;
    let manager = MCPManager::new(pool, None);
    manager
        .connect_server(&server_id)
        .await
        .expect("connect must survive non-JSON lines on stdout");

    assert!(
        manager.has_tool("echo_tool"),
        "tools must still be registered for a noisy server"
    );

    // Two calls: the second proves the transport is still live after the
    // first reply arrived interleaved with more junk.
    for expected in ["first", "second"] {
        let result = manager
            .execute_tool("echo_tool", &json!({"text": expected}), None)
            .await;
        assert!(!result.is_error, "unexpected error: {}", result.content);
        assert_eq!(result.content, expected);
    }
}
