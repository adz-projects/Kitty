//! Spawn + health-probe the goosed ACP server (`goose serve`).
//!
//! Goose 1.41.0 exposes ACP-over-HTTP/WebSocket via `goose serve` (there is no
//! legacy `goosed agent`). We pick a free port, generate a secret key, and pass
//! it via `GOOSE_SERVER__SECRET_KEY` so the key never reaches the webview.
//! The ACP request/response wiring itself lands in Phase 2 (`goosed/api.rs`).

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use rand::Rng;

use crate::state::GoosedHandle;
use crate::state::ManagedProcess;
use crate::util::{capture_output, hidden_command};

/// Locate the `goose` binary: a config-persisted `goose_binary_override`
/// (set by the wizard's one-click install or its manual "point at an
/// existing install" fallback) first, then `GOOSE_BIN` env, then the Goose
/// Desktop bundle's `resources/bin/goose.exe`, then bare `goose` on PATH.
pub fn locate_goose(override_path: Option<&str>) -> PathBuf {
    if let Some(p) = override_path {
        let path = PathBuf::from(p);
        if path.exists() {
            return path;
        }
    }
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
    (0..32)
        .map(|_| format!("{:x}", rng.gen_range(0u8..16)))
        .collect()
}

/// Spawn `goose serve` on a free port and wait (briefly) for it to bind. `env`
/// carries provider/model overrides from the active profile (see providers.rs).
///
/// Round-4 item 4 ("provider switch sometimes very long") was diagnosed here
/// with temporary timing instrumentation: a live provider switch consistently
/// measured ~500-540ms total for kill+respawn+readiness, regardless of
/// source/destination provider — this path is not the bottleneck. The
/// reported slowness is the first real inference call to the newly-active
/// provider (e.g. OpenRouter routing/model cold-start), which happens inside
/// goosed's own outbound request and is outside Kitty's control to fix.
pub async fn spawn(
    env: Vec<(String, String)>,
    goose_binary_override: Option<&str>,
) -> Result<GoosedHandle, String> {
    let bin = locate_goose(goose_binary_override);
    let port = free_port().map_err(|e| format!("no free port: {e}"))?;
    let secret = generate_secret();

    let mut child = hidden_command(&bin)
        .arg("serve")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .env("GOOSE_SERVER__SECRET_KEY", &secret)
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn `goose serve` ({}): {e}", bin.display()))?;
    capture_output(&mut child, "goosed");

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
