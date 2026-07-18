//! Process lifecycle for the Adaptive Pathway extension's HTTP sidecar
//! (`integrations/sidecar/server.py` in that separate repo, launched via its
//! `adaptive-pathway-sidecar` console script). Modeled directly on
//! `ollama_proc.rs`: probe first, only spawn (and only ever kill) what we
//! actually started ourselves.
//!
//! Kept deliberately separate from `StackStatus`/`spawn_health_loop` — this
//! sidecar is an optional augmentation (missing hints shouldn't read as a
//! `goosed_down`-class outage), so it gets its own small status + event.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::state::ManagedProcess;
use crate::util::{capture_output, hidden_command};

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
