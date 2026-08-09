//! Spawn + health-probe the BigTiny daemon.
//!
//! We pick a free port, generate a secret, and pass it via `BIGTINY_SECRET`
//! so the key never reaches the webview; every client call sends it back as
//! `X-API-Key`. `/api/health` is exempt from auth on the BigTiny side, so
//! readiness polling needs no key.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rand::Rng;
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};

use crate::state::DaemonHandle;
use crate::state::ManagedProcess;
use crate::util::{capture_output, hidden_command};

/// Name fragment of the frozen daemon exe's process — used only to confirm a
/// pidfile's PID still looks like our own daemon before killing it (a
/// recycled PID could otherwise belong to an unrelated process started
/// later).
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
/// names one that's still alive: a graceful-exit-only cleanup path leaves
/// this process orphaned across a `tauri dev` hot-restart or a terminal
/// Ctrl+C, which then locks the frozen exe file against the next build. A
/// normal launch only ever calls `spawn` once, so finding a pidfile here
/// always means "leftover from a run that already ended."
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

/// Pick an unused localhost port and hold the bound socket so it stays
/// reserved until just before the child is spawned. The old helper bound to
/// `:0`, read the port, then *dropped* the socket — a TOCTOU where another
/// process could grab that port before BigTiny bound it, and Kitty's health
/// probe would then be answered by an unrelated process on the same port.
/// Holding the listener here keeps the port reserved for the whole (non-trivial)
/// setup below and releases it only immediately before `cmd.spawn()`.
fn bind_reserved_port() -> std::io::Result<(u16, std::net::TcpListener)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    Ok((port, listener))
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
#[allow(clippy::too_many_arguments)]
pub async fn spawn(
    command: &str,
    args: &[String],
    dir: Option<&str>,
    summarizer: &crate::config::SummarizerSettings,
    token_management: &crate::config::TokenManagementSettings,
    memory: &crate::config::MemorySettings,
    local: &crate::config::LocalModelSettings,
    pathway_enabled: bool,
    pathway_embedding_model: &str,
) -> Result<DaemonHandle, String> {
    kill_stale_orphan();

    let (port, _reserved) = bind_reserved_port().map_err(|e| format!("no free port: {e}"))?;
    let secret = generate_secret();
    // Unlike `secret` above (regenerated every launch), this must be stable
    // across restarts or previously-encrypted rows in BigTiny's own DB
    // become undecryptable — stored in Windows Credential Manager, not
    // regenerated here. A Credential Manager failure is a hard error: it
    // must never silently fall through to BigTiny's own standalone-mode key
    // file fallback just because Kitty's own lookup failed (that fallback
    // exists for genuinely standalone runs with no Kitty parent process,
    // not as a safety net for this).
    let encryption_key = tokio::task::spawn_blocking(
        crate::config::providers::get_or_create_bigtiny_encryption_key,
    )
    .await
    .map_err(|e| format!("encryption key task panicked: {e}"))??;

    let mut cmd = hidden_command(Path::new(command));
    cmd.args(args)
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .env("BIGTINY_SECRET", &secret)
        .env("BIGTINY_ENCRYPTION_KEY", &encryption_key)
        // BigTiny's own env-var config surface (`BigTinyConfig`'s
        // `env_nested_delimiter="__"`, `plugins/bigtiny/bigtiny/config.py`) —
        // Kitty never writes a --config YAML for the daemon, so this is the
        // only path these settings reach it by.
        .env(
            "BIGTINY_SUMMARIZER__ENABLED",
            if summarizer.enabled { "true" } else { "false" },
        )
        // No `MODEL`/`KEEP_ALIVE` any more: those named an Ollama tag and an
        // Ollama-native keep-alive value for the retired `SummarizerClient`.
        // The local summarizer's model comes from `BIGTINY_LOCAL__MODEL_PATH`
        // below instead (see docs/ANDROID.md §10 Phase 4.1).
        .env(
            "BIGTINY_TOKEN_MANAGEMENT__MAX_CONTEXT_TOKENS",
            token_management.max_context_tokens.to_string(),
        )
        .env(
            "BIGTINY_TOKEN_MANAGEMENT__MAX_LIVE_TAIL_TOKENS",
            token_management.max_live_tail_tokens.to_string(),
        )
        .env(
            "BIGTINY_TOKEN_MANAGEMENT__MESSAGE_MASK_HEAD_LINES",
            token_management.message_mask_head_lines.to_string(),
        )
        .env(
            "BIGTINY_TOKEN_MANAGEMENT__MESSAGE_MASK_TAIL_LINES",
            token_management.message_mask_tail_lines.to_string(),
        )
        // `PathwayConfig::enabled` defaults to `false` inside BigTiny and
        // (unlike every other config section) previously had no env
        // override at all — passing a `--config` YAML is the only other way
        // to reach it, which Kitty never does. Without this, the in-process
        // behavioral-memory engine can never actually turn on, regardless
        // of anything else.
        .env(
            "BIGTINY_PATHWAY__ENABLED",
            if pathway_enabled { "true" } else { "false" },
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The in-process llama.cpp engine (docs/ANDROID.md §3). `--config` is
    // never passed, so env is the only channel these reach the daemon by —
    // `bin/bigtiny_daemon.rs::apply_env_overrides` is the other half of this
    // and must stay in lockstep.
    //
    // Paths are resolved here rather than in the daemon so the daemon never
    // needs to know where Kitty keeps models. An unresolvable name yields an
    // empty value, which the daemon reads as "that slot is unconfigured" —
    // chat falls back to the active provider, embeddings to lexical hashing.
    // Both are degradations, neither is an error.
    let chat_gguf = crate::models::resolve(&summarizer.model)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let embed_gguf = crate::models::resolve(pathway_embedding_model)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let local_enabled = !chat_gguf.is_empty() || !embed_gguf.is_empty();
    cmd.env(
        "BIGTINY_LOCAL__ENABLED",
        if local_enabled { "true" } else { "false" },
    )
    .env("BIGTINY_LOCAL__MODEL_PATH", &chat_gguf)
    .env("BIGTINY_LOCAL__EMBED_MODEL_PATH", &embed_gguf)
    // The engine's tunable knobs (docs/ANDROID.md §3.2/§6.1) — always sent,
    // not just when a model is present, so a model added later (e.g. the
    // daemon self-heals `LocalModelMissing` without a restart in between)
    // starts with the user's chosen settings rather than the daemon's own
    // hardcoded defaults.
    .env("BIGTINY_LOCAL__N_CTX", local.n_ctx.to_string())
    .env("BIGTINY_LOCAL__EMBED_N_CTX", local.embed_n_ctx.to_string())
    .env("BIGTINY_LOCAL__N_BATCH", local.n_batch.to_string())
    .env("BIGTINY_LOCAL__N_THREADS", local.n_threads.to_string())
    .env("BIGTINY_LOCAL__N_GPU_LAYERS", local.n_gpu_layers.to_string())
    .env("BIGTINY_LOCAL__EMBED_POOLING", &local.embed_pooling)
    .env("BIGTINY_LOCAL__CACHE_TYPE_K", &local.cache_type_k)
    .env("BIGTINY_LOCAL__CACHE_TYPE_V", &local.cache_type_v);
    if !local_enabled {
        tracing::info!(
            summarizer_model = %summarizer.model,
            embedding_model = %pathway_embedding_model,
            "no local GGUF found for either slot; the local engine stays off"
        );
    }

    // Relay the bm25 gate only when actually set — leaving the env absent
    // (rather than `""`) keeps `None` the daemon's truly-unset default and
    // avoids always-present env noise.
    if let Some(threshold) = memory.bm25_threshold {
        cmd.env("BIGTINY_MEMORY__BM25_THRESHOLD", threshold.to_string());
    }
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
    // Release the reserved port now, immediately before the child binds —
    // this is the whole point of `bind_reserved_port` (hold through setup,
    // release at the instant of spawn).
    drop(_reserved);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn BigTiny ({command}): {e}"))?;
    write_pidfile(child.id());
    capture_output(&mut child, "bigtiny");

    let client = crate::util::http_client();
    let mut healthy = false;
    for _ in 0..60 {
        if probe_health(&client, port).await {
            healthy = true;
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
        healthy,
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
        assert_eq!(
            std::fs::read_to_string(pidfile_path(&dir)).unwrap(),
            "12345"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
