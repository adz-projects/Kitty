//! Ollama process detection + (conditional) spawning and health probes.
//! We never call generate/chat here — inference goes through goosed. We only
//! manage the process and read `/api/version` and `/api/tags`.

use std::path::PathBuf;
use std::process::Command;

use crate::lifecycle::ManagedProcess;
use crate::util::hidden_command;

/// `GET /api/version` — treat any successful HTTP response as "up".
pub async fn probe_version(client: &reqwest::Client, base_url: &str) -> bool {
    let url = format!("{}/api/version", base_url.trim_end_matches('/'));
    client.get(url).send().await.is_ok()
}

/// `GET /api/tags` — true if at least one model is installed.
pub async fn has_any_model(client: &reqwest::Client, base_url: &str) -> bool {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    match client.get(url).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(json) => json
                .get("models")
                .and_then(|m| m.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false),
            Err(_) => false,
        },
        Err(_) => false,
    }
}

/// Locate the `ollama` binary: `OLLAMA_BIN` override, the default per-user
/// install dir, then bare `ollama` on PATH.
pub fn locate_ollama() -> PathBuf {
    if let Ok(p) = std::env::var("OLLAMA_BIN") {
        let path = PathBuf::from(p);
        if path.exists() {
            return path;
        }
    }
    if let Some(local) = dirs::data_local_dir() {
        let candidate = local.join("Programs").join("Ollama").join("ollama.exe");
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from("ollama")
}

/// Ensure Ollama is reachable. If already up, we do not own it. If down and a
/// binary exists, spawn `ollama serve` and mark it owned.
pub async fn ensure_running(base_url: &str) -> Result<ManagedProcess, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .map_err(|e| e.to_string())?;

    if probe_version(&client, base_url).await {
        // Already running — not ours; never kill it.
        return Ok(ManagedProcess {
            child: None,
            owned: false,
        });
    }

    let bin = locate_ollama();
    let child = hidden_command(&bin)
        .arg("serve")
        .spawn()
        .map_err(|e| format!("failed to spawn ollama serve ({}): {e}", bin.display()))?;

    // Give it a moment to bind before the first health probe.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    Ok(ManagedProcess {
        child: Some(child),
        owned: true,
    })
}

/// Kept for symmetry / future use: whether a bare `ollama` resolves on PATH.
#[allow(dead_code)]
pub fn ollama_on_path() -> bool {
    Command::new("ollama").arg("--version").output().is_ok()
}
