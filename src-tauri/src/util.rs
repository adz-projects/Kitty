//! Small cross-cutting helpers.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command};
use std::sync::OnceLock;

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// One process-wide `reqwest::Client`, built on first use. Every call site
/// that used to build its own client (`Client::builder()...build()`,
/// `Client::new()`, or the bare `reqwest::get`/`reqwest::Client::new()`
/// one-offs scattered across `ollama/`, `openrouter/`, `adaptive_pathway/`,
/// `lifecycle/`, `config/providers.rs`, and `wizard.rs`) now clones this
/// instead — a clone is a cheap `Arc` bump, while building a fresh client
/// re-initializes TLS/connection-pool state and throws away keep-alive.
pub fn http_client() -> reqwest::Client {
    HTTP_CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent("kitty-app")
                .build()
                .expect("reqwest client")
        })
        .clone()
}

/// Build a [`Command`] that does not flash a console window on Windows.
pub fn hidden_command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Forward a managed child's stdout/stderr into our own tracing log, each
/// line prefixed with `tag` (e.g. `"goosed"`). Without this, a crash/panic
/// inside a child process we spawned (goosed, Ollama, the Adaptive Pathway
/// sidecar) is completely invisible — all Kitty itself ever sees is a
/// downstream symptom (e.g. `goosed/api.rs`'s "ACP websocket closed"), with
/// no indication of the actual cause, since none of these commands piped
/// their output anywhere before (confirmed real report: a goosed crash left
/// only that one generic line in the log). Call this right after `spawn()`,
/// with the command having been built with
/// `.stdout(Stdio::piped()).stderr(Stdio::piped())` — takes the pipes as soon
/// as the child exists so nothing is missed, and reads them on plain OS
/// threads (not tokio tasks) since this is blocking, line-buffered I/O with
/// no async runtime dependency of its own.
pub fn capture_output(child: &mut Child, tag: &'static str) {
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                tracing::info!("{tag}: {line}");
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                tracing::warn!("{tag}: {line}");
            }
        });
    }
}
