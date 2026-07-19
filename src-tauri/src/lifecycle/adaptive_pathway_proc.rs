//! Process lifecycle for the Adaptive Pathway extension's HTTP sidecar
//! (`integrations/sidecar/server.py` in that separate repo, launched via its
//! `adaptive-pathway-sidecar` console script). Modeled directly on
//! `ollama_proc.rs`: probe first, only spawn (and only ever kill) what we
//! actually started ourselves.
//!
//! Kept deliberately separate from `StackStatus`/`spawn_health_loop` — this
//! sidecar is an optional augmentation (missing hints shouldn't read as a
//! `goosed_down`-class outage), so it gets its own small status + event.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};

use crate::state::ManagedProcess;
use crate::util::{capture_output, hidden_command};

/// Name fragment of the frozen sidecar exe's process — used only to confirm
/// a pidfile's PID still looks like our own sidecar before killing it (a
/// recycled PID could otherwise belong to an unrelated process started
/// later). Case-insensitive substring match against the actual frozen exe
/// name (`adaptive-pathway-sidecar-<target-triple>.exe`, see `plugins/build.py`).
const SIDECAR_PROCESS_NAME_FRAGMENT: &str = "adaptive-pathway-sidecar";

fn pidfile_path(db_path: &str) -> Option<PathBuf> {
    Path::new(db_path)
        .parent()
        .map(|p| p.join("adaptive-pathway-sidecar.pid"))
}

/// Kills a stale orphan from a *previous* run of this app, if the pidfile
/// next to `db_path` names one that's still alive.
///
/// `kill_if_owned()` (called from `lifecycle::shutdown` on `RunEvent::Exit`)
/// only runs on a *graceful* app exit. It never runs when the process is
/// terminated directly at the OS level instead — which is exactly what
/// happens both on a terminal Ctrl+C during `tauri dev` and on every
/// dev-mode hot-restart tauri-cli performs when it detects a source change
/// (it kills the previous `cargo run` child outright, not through Tauri's
/// event loop) — so the sidecar this app spawned is orphaned every time,
/// with no chance to clean itself up. Left running, it keeps the frozen exe
/// file locked, breaking the *next* `cargo build`'s attempt to overwrite it
/// (observed repeatedly). Since a normal app launch only ever calls
/// `ensure_running` once, finding a pidfile here always means "leftover from
/// a run that already ended" — never a concurrent instance — so killing a
/// confirmed match is safe.
fn kill_stale_orphan(db_path: &str) {
    let Some(path) = pidfile_path(db_path) else {
        return;
    };
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
        if name.contains(SIDECAR_PROCESS_NAME_FRAGMENT) {
            process.kill();
        }
        // A live PID whose name doesn't match is some unrelated process that
        // happens to have reused the number — never touch it, but the
        // pidfile is stale either way, so still clear it below.
    }
    let _ = std::fs::remove_file(&path);
}

fn write_pidfile(db_path: &str, pid: u32) {
    if let Some(path) = pidfile_path(db_path) {
        let _ = std::fs::write(path, pid.to_string());
    }
}

/// Removes the pidfile once the sidecar we own has actually been killed —
/// called from `lifecycle::shutdown`. A no-op if we never owned it (nothing
/// was ever written) or the file's already gone.
pub fn remove_pidfile(db_path: &str) {
    if let Some(path) = pidfile_path(db_path) {
        let _ = std::fs::remove_file(path);
    }
}

/// Status of the Adaptive Pathway sidecar, surfaced in Settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AdaptivePathwayStatus {
    /// Feature toggled off in config — no process, no health checks.
    #[default]
    Disabled,
    /// Enabled, spawn/probe in flight.
    Starting,
    Ok,
    /// Enabled but unreachable.
    Down,
}

/// Readiness of the shared embedding model (`qwen3-embedding:0.6b` by
/// default) that gives adaptive-pathway real context vectors instead of the
/// lexical-hashing fallback. Deliberately **not** folded into
/// `AdaptivePathwayStatus`: the sidecar can be perfectly `Ok` (reachable,
/// serving hints) while this is `Missing`/`Downloading` — it degrades
/// gracefully to hashing embeddings rather than reading as an outage. Never
/// touches `StackStatus` (chat readiness stays independent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingModelStatus {
    /// Not yet checked (e.g. adaptive-pathway disabled, or Ollama unreachable).
    #[default]
    Unknown,
    /// The pinned tag is installed and ready.
    Present,
    /// A background `ollama pull` is in flight (progress via the existing
    /// `ollama://pull-progress` events, keyed by `pull_id`).
    Downloading,
    /// Checked and not installed; no pull currently running (e.g. Ollama is
    /// down, or the pull attempt failed).
    Missing,
}

/// `GET /health` — any successful HTTP response counts as "up" (mirrors
/// `ollama_proc::probe_version`).
pub async fn probe_health(client: &reqwest::Client, base_url: &str) -> bool {
    let url = format!("{}/health", base_url.trim_end_matches('/'));
    client
        .get(url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .is_ok()
}

/// Ensure the sidecar is reachable at `http://127.0.0.1:{port}`. If already up,
/// we don't own it (never touch a pre-started instance). If down, spawn
/// `launch_command` (plus any extra `launch_args`) with `--db-path`/`--port`
/// appended, and poll briefly for it to come up before returning.
///
/// `embedding_model`/`embedding_url` are passed as `AP_EMBED_OLLAMA_MODEL`/
/// `AP_EMBED_OLLAMA_URL` so the sidecar's `EmbeddingProvider` uses the same
/// tag and endpoint Kitty pulls via Ollama — keeping the two independently-
/// spawned Python processes (this sidecar and the goosed-spawned MCP
/// extension) from drifting to different embedding spaces or endpoints.
pub async fn ensure_running(
    launch_command: &str,
    launch_args: &[String],
    db_path: &str,
    port: u16,
    embedding_model: &str,
    embedding_url: &str,
) -> Result<ManagedProcess, String> {
    kill_stale_orphan(db_path);

    let client = crate::util::http_client();
    let base = format!("http://127.0.0.1:{port}");

    if probe_health(&client, &base).await {
        return Ok(ManagedProcess {
            child: None,
            owned: false,
        });
    }

    // The default db_path is now an absolute path under a Kitty-owned
    // directory (e.g. %LOCALAPPDATA%/adaptive-pathway/) that may not exist
    // yet on first run — SQLite won't create a missing parent directory.
    if let Some(parent) = Path::new(db_path).parent() {
        let parent = parent.to_path_buf();
        let _ = tokio::task::spawn_blocking(move || std::fs::create_dir_all(&parent)).await;
    }

    let mut child = hidden_command(Path::new(launch_command))
        .args(launch_args)
        .arg("--db-path")
        .arg(db_path)
        .arg("--port")
        .arg(port.to_string())
        .env("AP_EMBED_OLLAMA_MODEL", embedding_model)
        .env("AP_EMBED_OLLAMA_URL", embedding_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn `{launch_command}`: {e}"))?;
    write_pidfile(db_path, child.id());
    capture_output(&mut child, "adaptive-pathway");

    // uvicorn's startup is a bit slower than a bare TCP bind — poll briefly
    // rather than a single fixed sleep (mirrors goosed::spawn's readiness loop).
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(300)).await;
        if probe_health(&client, &base).await {
            break;
        }
    }

    Ok(ManagedProcess {
        child: Some(child),
        owned: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, unique-per-test directory under the OS temp dir, standing in
    /// for adaptive-pathway's real `%LOCALAPPDATA%/adaptive-pathway/` folder —
    /// `db_path` only needs a parent directory to exist for `pidfile_path` to
    /// resolve, it's never actually opened as a database here.
    fn temp_db_path(label: &str) -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!(
            "kitty-ap-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("adaptive_pathway.db");
        (dir, db_path.to_string_lossy().into_owned())
    }

    #[test]
    fn pidfile_path_sits_next_to_db_path() {
        let (dir, db_path) = temp_db_path("path");
        let pidfile = pidfile_path(&db_path).unwrap();
        assert_eq!(pidfile, dir.join("adaptive-pathway-sidecar.pid"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kill_stale_orphan_is_a_no_op_when_no_pidfile_exists() {
        let (dir, db_path) = temp_db_path("missing");
        // Should not panic or create anything.
        kill_stale_orphan(&db_path);
        assert!(!pidfile_path(&db_path).unwrap().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kill_stale_orphan_removes_pidfile_naming_a_dead_pid() {
        let (dir, db_path) = temp_db_path("dead");
        // A pid that's guaranteed not to be alive: spawn, kill, and wait on a
        // throwaway process rather than guessing a number.
        let mut child = std::process::Command::new("ping")
            .args(["-n", "1", "127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let dead_pid = child.id();
        let _ = child.wait();

        std::fs::write(pidfile_path(&db_path).unwrap(), dead_pid.to_string()).unwrap();
        kill_stale_orphan(&db_path);
        assert!(!pidfile_path(&db_path).unwrap().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kill_stale_orphan_never_touches_a_live_process_with_a_non_matching_name() {
        // Guards the safety property this whole mechanism depends on: a
        // recycled PID belonging to some unrelated live process must never
        // be killed just because a stale pidfile happens to name it.
        let (dir, db_path) = temp_db_path("live-mismatch");
        let mut child = std::process::Command::new("ping")
            .args(["-n", "15", "127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();

        std::fs::write(pidfile_path(&db_path).unwrap(), pid.to_string()).unwrap();
        kill_stale_orphan(&db_path);

        // Not killed (ping's name doesn't contain "adaptive-pathway-sidecar").
        assert!(
            child.try_wait().unwrap().is_none(),
            "an unrelated live process must not be killed"
        );
        // Pidfile is still cleared, since it was stale regardless.
        assert!(!pidfile_path(&db_path).unwrap().exists());

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_pidfile_then_remove_pidfile_round_trips() {
        let (dir, db_path) = temp_db_path("roundtrip");
        write_pidfile(&db_path, 12345);
        let pidfile = pidfile_path(&db_path).unwrap();
        assert_eq!(std::fs::read_to_string(&pidfile).unwrap(), "12345");
        remove_pidfile(&db_path);
        assert!(!pidfile.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
