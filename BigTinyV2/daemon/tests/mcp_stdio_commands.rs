//! How a user-entered stdio `command` is resolved. `fake_mcp_server` stands in
//! for a real server, reached through the shapes people actually type:
//! an absolute exe path, a bare exe name on PATH, and (Windows) a `.cmd`
//! shim such as `npx` / `npx.cmd`, by path and by bare name.

use std::path::PathBuf;

use bigtiny2::mcp::MCPManager;
use serde_json::json;
use sqlx::SqlitePool;

async fn pool_with(command: &str, args: &str, env: Option<&str>) -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO mcp_servers (id, name, transport, command, args, env, enabled, status) \
         VALUES ('s', 's', 'stdio', ?, ?, ?, 1, 'disconnected')",
    )
    .bind(command)
    .bind(args)
    .bind(env)
    .execute(&pool)
    .await
    .unwrap();
    pool
}

async fn roundtrip(command: &str, args: &str, env: Option<&str>) -> Result<(), String> {
    let manager = MCPManager::new(pool_with(command, args, env).await, None);
    manager
        .connect_server("s")
        .await
        .map_err(|e| e.to_string())?;
    let r = manager
        .execute_tool("echo_tool", &json!({"text": "hi"}), None)
        .await;
    if r.is_error {
        return Err(format!("tool call failed: {r:?}"));
    }
    Ok(())
}

fn fake_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake_mcp_server"))
}

/// A fresh directory holding only what the test puts there.
fn scratch_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bt2-stdio-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The server's `env` overlay with PATH = `dir` + the inherited PATH, so a
/// bare name resolves in `dir`. JSON object, as the `env` column stores it.
fn env_with_path(dir: &std::path::Path) -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };
    let path = format!(
        "{}{sep}{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    json!({ "PATH": path }).to_string()
}

#[tokio::test]
async fn absolute_exe_path() {
    roundtrip(fake_exe().to_str().unwrap(), "[]", None)
        .await
        .unwrap();
}

#[tokio::test]
async fn bare_exe_name_on_path() {
    let dir = fake_exe().parent().unwrap().to_path_buf();
    roundtrip("fake_mcp_server", "[]", Some(&env_with_path(&dir)))
        .await
        .unwrap();
}

#[cfg(windows)]
fn write_cmd_shim(dir: &std::path::Path, name: &str) -> PathBuf {
    let shim = dir.join(format!("{name}.cmd"));
    std::fs::write(
        &shim,
        format!("@echo off\r\n\"{}\" %*\r\n", fake_exe().display()),
    )
    .unwrap();
    shim
}

#[cfg(windows)]
#[tokio::test]
async fn cmd_shim_by_full_path() {
    let dir = scratch_dir("cmdpath");
    let shim = write_cmd_shim(&dir, "fakemcp");
    roundtrip(shim.to_str().unwrap(), "[]", None).await.unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn cmd_shim_by_name_with_extension() {
    let dir = scratch_dir("cmdext");
    write_cmd_shim(&dir, "fakemcp");
    roundtrip("fakemcp.cmd", "[]", Some(&env_with_path(&dir)))
        .await
        .unwrap();
}

/// What `npx` / `pnpm` / `yarn` are on Windows: a `.cmd` with no `.exe`
/// beside it, typed as a bare name.
#[cfg(windows)]
#[tokio::test]
async fn cmd_shim_by_bare_name() {
    let dir = scratch_dir("cmdbare");
    write_cmd_shim(&dir, "fakemcp");
    roundtrip("fakemcp", "[]", Some(&env_with_path(&dir)))
        .await
        .unwrap();
}

/// A directory but no extension (`C:\Program Files\nodejs\npx`), where an
/// extensionless sibling also exists — npm ships a POSIX `npx` script there.
#[cfg(windows)]
#[tokio::test]
async fn cmd_shim_by_path_without_extension() {
    let dir = scratch_dir("cmdnoext");
    write_cmd_shim(&dir, "fakemcp");
    std::fs::write(dir.join("fakemcp"), "#!/bin/sh\n").unwrap();
    roundtrip(dir.join("fakemcp").to_str().unwrap(), "[]", None)
        .await
        .unwrap();
}

/// Arguments must survive the trip through a `.cmd`.
#[cfg(windows)]
#[tokio::test]
async fn cmd_shim_passes_args() {
    let dir = scratch_dir("cmdargs");
    write_cmd_shim(&dir, "fakemcp");
    roundtrip("fakemcp", r#"["--noisy"]"#, Some(&env_with_path(&dir)))
        .await
        .unwrap();
}

/// A name that resolves to nothing still fails, with std's own error.
#[tokio::test]
async fn missing_program_still_fails_cleanly() {
    let err = roundtrip("no-such-mcp-server-xyz", "[]", None)
        .await
        .unwrap_err();
    assert!(
        err.contains("not found") || err.contains("No such file"),
        "{err}"
    );
}
