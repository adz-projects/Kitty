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
         VALUES (?, ?, 'stdio', ?, '[]', 1, 'disconnected')",
    )
    .bind(server_id)
    .bind("fake")
    .bind(command)
    .execute(&pool)
    .await
    .unwrap();

    (pool, server_id.to_string())
}

#[tokio::test]
async fn execute_tool_success_roundtrip() {
    let (pool, server_id) = setup_pool_with_server().await;
    let manager = MCPManager::new(pool);
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
    let manager = MCPManager::new(pool);
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
    let manager = MCPManager::new(pool);
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
    let manager = MCPManager::new(pool);
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
    let manager = MCPManager::new(pool);
    manager
        .connect_server(&server_id)
        .await
        .expect("connect should succeed");

    let result = manager
        .execute_tool("crash_tool", &json!({}), Some(Duration::from_secs(5)))
        .await;
    assert!(result.is_error);
}
