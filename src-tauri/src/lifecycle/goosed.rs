//! Spawn + health-probe the goosed ACP server (`goose serve`).
//!
//! Goose 1.41.0 exposes ACP-over-HTTP/WebSocket via `goose serve` (there is no
//! legacy `goosed agent`). We pick a free port, generate a secret key, and pass
//! it via `GOOSE_SERVER__SECRET_KEY` so the key never reaches the webview.
//! The ACP request/response wiring itself lands in Phase 2 (`goosed/api.rs`).

use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Duration;

use rand::Rng;

use crate::lifecycle::ManagedProcess;
use crate::state::GoosedHandle;
use crate::util::hidden_command;

/// Locate the `goose` binary: `GOOSE_BIN` override, the Goose Desktop bundle's
/// `resources/bin/goose.exe`, then bare `goose` on PATH.
fn locate_goose() -> PathBuf {
    if let Ok(p) = std::env::var("GOOSE_BIN") {
        let path = PathBuf::from(p);
        if path.exists() {
            return path;
        }
    }
    if let Some(local) = dirs::data_local_dir() {
        let candidate = local
            .join("Programs")
            .join("dist-windows")
            .join("resources")
            .join("bin")
            .join("goose.exe");
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from("goose")
}

/// Ask the OS for an unused localhost port by binding to :0 and reading it back.
fn free_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// 32 hex chars of randomness for `GOOSE_SERVER__SECRET_KEY`.
fn generate_secret() -> String {
    let mut rng = rand::thread_rng();
    (0..32).map(|_| format!("{:x}", rng.gen_range(0u8..16))).collect()
}

/// Spawn `goose serve` on a free port and wait (briefly) for it to bind.
pub async fn spawn() -> Result<GoosedHandle, String> {
    let bin = locate_goose();
    let port = free_port().map_err(|e| format!("no free port: {e}"))?;
    let secret = generate_secret();

    let child = hidden_command(&bin)
        .arg("serve")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .env("GOOSE_SERVER__SECRET_KEY", &secret)
        .spawn()
        .map_err(|e| format!("failed to spawn `goose serve` ({}): {e}", bin.display()))?;

    // Poll for the port to accept connections (up to ~10s).
    for _ in 0..40 {
        if is_up(port).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    Ok(GoosedHandle {
        process: ManagedProcess {
            child: Some(child),
            owned: true,
        },
        port: Some(port),
        secret_key: Some(secret),
    })
}

/// Cheap liveness check: can we open a TCP connection to the ACP port?
/// (A protocol-level probe replaces this once ACP methods are wired in Phase 2.)
pub async fn is_up(port: u16) -> bool {
    tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok()
}
