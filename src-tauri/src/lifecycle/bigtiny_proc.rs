//! Spawn + health-probe the BigTiny daemon.
//!
//! We pick a free port, generate a secret, and pass it via `BIGTINY_SECRET`
//! so the key never reaches the webview; every client call sends it back as
//! `X-API-Key`. `/api/health` is exempt from auth on the BigTiny side, so
//! readiness polling needs no key.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rand::Rng;
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};

use crate::state::DaemonHandle;
use crate::state::ManagedProcess;
use crate::util::{capture_output, hidden_command};

/// Name fragment of the frozen daemon exe's process — used only to confirm a
/// pidfile's PID still looks like our own daemon before killing it (mirrors
/// `adaptive_pathway_proc`'s identical safety check).
const DAEMON_PROCESS_NAME_FRAGMENT: &str = "bigtiny-daemon";

/// BigTiny's own consolidated data root (`%APPDATA%/Kitty/bigtiny/` — see
/// `config::bigtiny_data_dir`), the same directory `spawn` points
/// `BIGTINY_DATA_DIR` at — mirrors the sidecar's pidfile living next to its
/// own configurable db path, now that BigTiny has an equivalent anchor.
fn default_pidfile_dir() -> Option<PathBuf> {
    crate::config::bigtiny_data_dir().ok()
}

fn pidfile_path(dir: &Path) -> PathBuf {
    dir.join("bigtiny-daemon.pid")
}

/// Kills a stale orphan from a *previous* run of this app, if the pidfile
/// names one that's still alive — same rationale as
/// `adaptive_pathway_proc::kill_stale_orphan`: a graceful-exit-only cleanup
/// path leaves this process orphaned across a `tauri dev` hot-restart or a
/// terminal Ctrl+C, which then locks the frozen exe file against the next
/// build. A normal launch only ever calls `spawn` once, so finding a pidfile
/// here always means "leftover from a run that already ended."
fn kill_stale_orphan_in(dir: &Path) {
    let path = pidfile_path(dir);
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(pid) = contents.trim().parse::<u32>() else {
        let _ = std::fs::remove_file(&path);
        return;
    };

    let sys =
        System::new_with_specifics(RefreshKind::new().with_processes(ProcessRefreshKind::new()));
    if let Some(process) = sys.process(Pid::from_u32(pid)) {
        let name = process.name().to_string_lossy().to_lowercase();
        if name.contains(DAEMON_PROCESS_NAME_FRAGMENT) {
            process.kill();
        }
    }
    let _ = std::fs::remove_file(&path);
}

fn kill_stale_orphan() {
    if let Some(dir) = default_pidfile_dir() {
        kill_stale_orphan_in(&dir);
    }
}

fn write_pidfile_in(dir: &Path, pid: u32) {
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(pidfile_path(dir), pid.to_string());
}

fn write_pidfile(pid: u32) {
    if let Some(dir) = default_pidfile_dir() {
        write_pidfile_in(&dir, pid);
    }
}

/// Removes the pidfile once the daemon we own has actually been killed —
/// called from `lifecycle::shutdown`. A no-op if nothing was ever written or
/// the file's already gone.
pub fn remove_pidfile() {
    if let Some(dir) = default_pidfile_dir() {
        let _ = std::fs::remove_file(pidfile_path(&dir));
    }
}

/// Ask the OS for an unused localhost port by binding to :0 and reading it back.
fn free_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// 32 hex chars of randomness for `BIGTINY_SECRET`.
fn generate_secret() -> String {
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| format!("{:x}", rng.gen_range(0u8..16)))
        .collect()
}

/// Spawn the BigTiny daemon and wait (up to ~15s — a Python interpreter +
/// FastAPI import chain is slower to first-bind than a native binary) for its
/// health endpoint to answer.
pub async fn spawn(
    command: &str,
    args: &[String],
    dir: Option<&str>,
) -> Result<DaemonHandle, String> {
    kill_stale_orphan();

    let port = free_port().map_err(|e| format!("no free port: {e}"))?;
    let secret = generate_secret();

    let mut cmd = hidden_command(Path::new(command));
    cmd.args(args)
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .env("BIGTINY_SECRET", &secret)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Consolidates BigTiny's db/cache-sandbox-root/recipes under
    // %APPDATA%/Kitty/bigtiny/ instead of its own standalone `~/.bigtiny`
    // default — see `bigtiny/paths.py::data_dir()`. Best-effort: if this
    // can't be resolved for some reason, BigTiny just falls back to its own
    // standalone default rather than failing to spawn at all.
    if let Ok(data_dir) = crate::config::bigtiny_data_dir() {
        cmd.env("BIGTINY_DATA_DIR", &data_dir);
    }
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn BigTiny ({command}): {e}"))?;
    write_pidfile(child.id());
    capture_output(&mut child, "bigtiny");

    let client = crate::util::http_client();
    for _ in 0..60 {
        if probe_health(&client, port).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    Ok(DaemonHandle {
        process: ManagedProcess {
            child: Some(child),
            owned: true,
        },
        port: Some(port),
        secret_key: Some(secret),
    })
}

/// Protocol-level liveness: does `GET /api/health` answer 200? (Open without
/// the API key by design, exactly for this probe.)
pub async fn probe_health(client: &reqwest::Client, port: u16) -> bool {
    client
        .get(format!("http://127.0.0.1:{port}/api/health"))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, unique-per-test directory under the OS temp dir, standing in
    /// for `default_pidfile_dir()`'s real `%LOCALAPPDATA%/Kitty/` folder.
    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kitty-bigtiny-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn pidfile_path_sits_in_the_given_dir() {
        let dir = temp_dir("path");
        assert_eq!(pidfile_path(&dir), dir.join("bigtiny-daemon.pid"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kill_stale_orphan_is_a_no_op_when_no_pidfile_exists() {
        let dir = temp_dir("missing");
        kill_stale_orphan_in(&dir);
        assert!(!pidfile_path(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kill_stale_orphan_removes_pidfile_naming_a_dead_pid() {
        let dir = temp_dir("dead");
        let mut child = std::process::Command::new("ping")
            .args(["-n", "1", "127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let dead_pid = child.id();
        let _ = child.wait();

        std::fs::write(pidfile_path(&dir), dead_pid.to_string()).unwrap();
        kill_stale_orphan_in(&dir);
        assert!(!pidfile_path(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kill_stale_orphan_never_touches_a_live_process_with_a_non_matching_name() {
        let dir = temp_dir("live-mismatch");
        let mut child = std::process::Command::new("ping")
            .args(["-n", "15", "127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();

        std::fs::write(pidfile_path(&dir), pid.to_string()).unwrap();
        kill_stale_orphan_in(&dir);

        // Not killed (ping's name doesn't contain "bigtiny-daemon").
        assert!(
            child.try_wait().unwrap().is_none(),
            "an unrelated live process must not be killed"
        );
        assert!(!pidfile_path(&dir).exists());

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_pidfile_then_read_round_trips() {
        let dir = temp_dir("roundtrip");
        write_pidfile_in(&dir, 12345);
        assert_eq!(std::fs::read_to_string(pidfile_path(&dir)).unwrap(), "12345");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
