//! Stdio MCP server as a child process, on top of `HardenedRwTransport`.
//!
//! Replaces `rmcp::transport::TokioChildProcess`, which is hardwired to
//! rmcp's own `AsyncRwTransport` (see `rw_transport`'s module docs for the
//! three failure modes that motivated replacing it) and exposes no way to
//! swap the framing. The child-lifecycle behavior rmcp got right is
//! reproduced here deliberately and must not regress:
//!
//! - **No zombies.** The child is reaped with `kill().await` (which waits),
//!   never a bare `start_kill()`. `kill_on_drop(true)` additionally hands the
//!   child to tokio's process reaper if this struct is dropped without a
//!   `close()`.
//! - **Graceful shutdown first.** `close()` drops the writer — closing the
//!   child's stdin, the conventional "please exit" signal for a stdio MCP
//!   server — waits up to `SHUTDOWN_GRACE`, and only then kills.

use std::process::Stdio;
use std::time::Duration;

use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::RoleClient;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use super::rw_transport::HardenedRwTransport;

/// How long a child gets to exit on its own after its stdin closes, before
/// being killed. Matches rmcp's own `MAX_WAIT_ON_DROP_SECS`.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// The program to spawn for a user-entered stdio `command`.
///
/// On Windows, `Command::new` only ever appends `.exe` to a name without an
/// extension, so `npx`, `pnpm` and `yarn` — which ship as `.cmd` shims with no
/// `.exe` beside them — fail with "program not found", although that is the
/// command almost every published MCP config gives. This resolves such a name
/// the way a shell does: each `PATHEXT` extension (`.COM;.EXE;.BAT;.CMD` when
/// unset) in each `PATH` directory, in order, or beside the command when it
/// has a directory part. `path` is the child's PATH: the server's `env` overlay
/// when it sets one, else the daemon's.
///
/// Anything that already has an extension, or that nothing matches, is
/// returned unchanged, so the spawn error stays the one std would give. Rust
/// runs a resolved `.cmd`/`.bat` through `cmd.exe` with its own argument
/// escaping. Elsewhere this is the identity.
pub fn resolve_program(command: &str, path: Option<&std::ffi::OsStr>) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let given = std::path::Path::new(command);
        if given.extension().is_none() {
            let pathext = std::env::var("PATHEXT").unwrap_or_default();
            if let Some(found) = find_with_pathext(given, path, &pathext) {
                return found;
            }
        }
    }
    #[cfg(not(windows))]
    let _ = path;
    std::path::PathBuf::from(command)
}

/// The search behind [`resolve_program`], with `PATHEXT` passed in so it can
/// be tested without touching the process environment.
#[cfg(windows)]
fn find_with_pathext(
    given: &std::path::Path,
    path: Option<&std::ffi::OsStr>,
    pathext: &str,
) -> Option<std::path::PathBuf> {
    let exts: Vec<&str> = match pathext
        .split(';')
        .filter(|e| !e.is_empty())
        .collect::<Vec<_>>()
    {
        v if v.is_empty() => vec![".COM", ".EXE", ".BAT", ".CMD"],
        v => v,
    };
    let has_dir = given.parent().is_some_and(|p| !p.as_os_str().is_empty());
    let dirs: Vec<std::path::PathBuf> = if has_dir {
        vec![std::path::PathBuf::new()]
    } else {
        path.map(|p| std::env::split_paths(p).collect())
            .unwrap_or_default()
    };
    for dir in dirs {
        for ext in &exts {
            let mut name = given.as_os_str().to_owned();
            name.push(ext.to_ascii_lowercase());
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

pub struct ChildProcessTransport {
    child: Option<Child>,
    inner: HardenedRwTransport<ChildStdout, ChildStdin>,
}

impl ChildProcessTransport {
    /// Spawn `command` with piped stdin/stdout (stderr is inherited, so a
    /// server's diagnostics still reach the daemon log rather than filling an
    /// unread pipe and blocking the child).
    pub fn spawn(mut command: Command) -> std::io::Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);

        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("child stdout was not piped"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("child stdin was not piped"))?;

        Ok(Self {
            child: Some(child),
            inner: HardenedRwTransport::new(stdout, stdin),
        })
    }

    async fn graceful_shutdown(&mut self) -> std::io::Result<()> {
        // Close stdin first: that is what tells a well-behaved stdio server
        // to exit, and it lets the wait below usually win the race.
        self.inner.close_write().await;

        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        match tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await {
            Ok(Ok(status)) => {
                tracing::debug!("MCP child exited gracefully: {status}");
                Ok(())
            }
            Ok(Err(e)) => {
                tracing::warn!("error waiting for MCP child: {e}");
                // `kill` waits, so this still reaps rather than leaving a zombie.
                let _ = child.kill().await;
                Err(e)
            }
            Err(_) => {
                tracing::warn!(
                    "MCP child did not exit within {}s of stdin close; killing",
                    SHUTDOWN_GRACE.as_secs()
                );
                child.kill().await
            }
        }
    }
}

impl Transport<RoleClient> for ChildProcessTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.inner.send(item)
    }

    fn receive(
        &mut self,
    ) -> impl std::future::Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.inner.receive()
    }

    fn close(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        self.graceful_shutdown()
    }
}
