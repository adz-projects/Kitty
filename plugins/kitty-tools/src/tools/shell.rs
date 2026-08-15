//! `lean_shell` — Rust port of `lean_mcp.py`'s `shell` tool.
//!
//! Three Windows traps the base plan calls out explicitly:
//! 1. `Command::arg` applies MSVC argv quoting and mangles anything with a
//!    `"` in it — the command string must go through `raw_arg` so it reaches
//!    `cmd.exe /c` verbatim, matching Python's `shell=True`.
//! 2. Spawning `cmd.exe` without `CREATE_NO_WINDOW` pops a console window on
//!    every call — a *new* symptom the port would otherwise introduce.
//! 3. `text=True` in Python decodes with the locale's preferred encoding
//!    (typically cp1252); UTF-8-lossy is a deliberate deviation — more
//!    correct going forward, and it keeps the golden corpus machine-
//!    independent instead of tracking a codepage.
//!
//! Two hardening changes over the Python original:
//! - The child is spawned with `kill_on_drop(true)`, so when the
//!   `tokio::time::timeout` fires and the child future is dropped, tokio
//!   kills the process instead of leaking it (Python's `subprocess.run` with
//!   `timeout=` kills on expiry). On timeout the whole process **tree** is
//!   additionally killed (`taskkill /T /F` on Windows) — `kill_on_drop`
//!   alone reaps only the direct `cmd.exe`, orphaning any grandchildren it
//!   spawned (audit #117).
//! - stdout/stderr are streamed through a bounded capture
//!   (`SHELL_MAX_CAPTURE_BYTES`) rather than fully buffered by
//!   `wait_with_output`, so a command spewing hundreds of MB can't blow RAM
//!   before the response's 100-line cut.
//!
//! Non-Windows builds get a plain `/bin/sh -c` fallback below (no MSVC
//! `raw_arg` quoting trap, no console window to suppress) so the crate can
//! at least type-check and run on Linux/macOS dev machines. This tool is not
//! registered at all on Android (see `server.rs`'s `shell_tool_router` /
//! `KittyToolsServer::new`) — an app-sandbox shell backed by toybox isn't a
//! useful `lean_shell` for a model to drive, and it's the tool with the
//! widest blast radius against `agent::sandbox`'s path-containment check on
//! the daemon side, which itself fails open for argument shapes it doesn't
//! recognize.

#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::Stdio;
use std::time::Duration;

use crate::envelope::{error_response, success_response};
use crate::text::py_splitlines;
use serde_json::json;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const SHELL_TIMEOUT: Duration = Duration::from_secs(30);
const SHELL_MAX_LINES: usize = 100;
const SHELL_KEEP_HEAD: usize = 30;
const SHELL_KEEP_TAIL: usize = 30;
/// Per-stream capture cap — a command spewing hundreds of MB is truncated at
/// the source (kept draining so it never blocks, but not retained) instead of
/// being fully buffered by `wait_with_output` before the 100-line cut.
const SHELL_MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;

fn strip_ansi(text: &str) -> String {
    // Verbatim port of `\x1B(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])` — note the
    // Python-class trap: inside `[@-Z\\-_]`, `\\-_` is the *range* `\x5C`-
    // `\x5F`, not a literal backslash then `-_`. Written explicitly here.
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\x1B(?:[@-Z\x5C-\x5F]|\[[0-?]*[ -/]*[@-~])").unwrap()
    });
    re.replace_all(text, "").into_owned()
}

/// Reads `reader` to EOF keeping at most `max_bytes`, draining any excess so
/// the child never blocks on a full pipe. Returns the captured bytes and
/// whether the cap was hit.
async fn read_bounded<R>(reader: Option<R>, max_bytes: usize) -> (Vec<u8>, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let Some(mut reader) = reader else {
        return (Vec::new(), false);
    };
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        let room = max_bytes.saturating_sub(buf.len());
        if room > 0 {
            let take = room.min(n);
            buf.extend_from_slice(&chunk[..take]);
            if take < n {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
    (buf, truncated)
}

pub async fn shell(command: &str, dry_run: bool) -> String {
    shell_impl(command, dry_run, SHELL_TIMEOUT).await.0
}

/// Runs a command with an explicit timeout (so tests don't wait 30s) and
/// returns the child's pid alongside the envelope so tests can assert the
/// process is actually reaped on timeout.
async fn shell_impl(command: &str, dry_run: bool, timeout: Duration) -> (String, Option<u32>) {
    if dry_run {
        return (
            success_response(
                json!({"command": command}),
                Some("[DRY RUN] Command not executed."),
                false,
                None,
            ),
            None,
        );
    }

    #[cfg(windows)]
    let mut std_cmd = {
        let mut c = std::process::Command::new("cmd");
        c.raw_arg(format!("/c {command}"));
        c.creation_flags(CREATE_NO_WINDOW);
        c
    };
    #[cfg(not(windows))]
    let mut std_cmd = {
        let mut c = std::process::Command::new("/bin/sh");
        c.arg("-c").arg(command);
        c
    };
    std_cmd.stdout(Stdio::piped());
    std_cmd.stderr(Stdio::piped());
    std_cmd.stdin(Stdio::null());

    // `kill_on_drop(true)` is the last-resort guard: any early return or
    // dropped future kills the process, so no orphaned shell is left
    // running. The timeout path kills the process *tree* explicitly (see
    // the `Err` arm below) before this backstop is needed.
    let mut child = match tokio::process::Command::from(std_cmd).kill_on_drop(true).spawn() {
        Ok(c) => c,
        Err(e) => {
            return (error_response("SHELL_SPAWN_ERROR", &format!("Failed to spawn shell: {e}"), None, None), None);
        }
    };
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Read both pipes to (bounded) EOF *concurrently* — draining them
    // sequentially can deadlock when one pipe fills while the child blocks
    // writing to the other. The child itself is deliberately NOT part of
    // this future: on timeout it must still be owned below so the tree kill
    // lands on a live root (see the `Err` arm).
    let capture = async {
        tokio::join!(
            read_bounded(stdout, SHELL_MAX_CAPTURE_BYTES),
            read_bounded(stderr, SHELL_MAX_CAPTURE_BYTES),
        )
    };

    match tokio::time::timeout(timeout, capture).await {
        Ok(((out_bytes, out_truncated), (err_bytes, err_truncated))) => {
            // Both pipes hit EOF, so the child has exited (no grandchild
            // holds a write end); `wait` just collects the status.
            let status = match child.wait().await {
                Ok(s) => s,
                Err(e) => {
                    return (
                        error_response("SHELL_SPAWN_ERROR", &format!("Failed to run shell: {e}"), None, None),
                        pid,
                    );
                }
            };
            let output = std::process::Output {
                status,
                stdout: out_bytes,
                stderr: err_bytes,
            };
            (finish(output, out_truncated || err_truncated), pid)
        }
        Err(_) => {
            // `kill_on_drop` reaps only the direct `cmd.exe`; take down the
            // whole tree so grandchildren don't outlive the timeout (audit
            // #117). This runs *before* the child is dropped, while the root
            // is still alive for `taskkill /T` to walk from.
            if let Some(pid) = pid {
                kill_process_tree(pid).await;
            }
            // Reap (bounded, so a failed kill can never hang the tool);
            // `kill_on_drop` remains the backstop beyond that.
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            (
                error_response(
                    "SHELL_TIMEOUT",
                    "Command timed out after 30s",
                    None,
                    Some("Try a faster approach, increase timeout, or break work into smaller commands."),
                ),
                pid,
            )
        }
    }
}

/// Kills the process tree rooted at `pid`, best-effort. On Windows this is
/// `taskkill /T /F` (`/T` walks the child tree, `/F` forces); errors are
/// ignored because the pid may already be gone — `kill_on_drop` races us
/// here, and `taskkill /T` on a dead root kills nothing, which is the one
/// grandchild-orphan case this does not cover (a Job Object would; noted
/// for the record, not worth the extra `windows-sys` dependency today).
#[cfg(windows)]
async fn kill_process_tree(pid: u32) {
    let mut cmd = std::process::Command::new("taskkill");
    cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    cmd.stdin(Stdio::null());
    let _ = tokio::process::Command::from(cmd).status().await;
}

/// Non-Windows fallback: the child is `/bin/sh -c`, which typically `exec`s
/// simple commands — so `kill_on_drop` already reaps the real process, and
/// there is no detached grandchild tree in the common case. A true tree
/// kill would need the child in its own process group (`setsid` at spawn)
/// followed by a group `kill`; not set up today, so this is documented
/// rather than half-done.
#[cfg(not(windows))]
async fn kill_process_tree(pid: u32) {
    let _ = pid;
}

fn finish(output: std::process::Output, capture_truncated: bool) -> String {
    let returncode = output.status.code().unwrap_or(-1);

    if returncode != 0 {
        let stderr_raw = String::from_utf8_lossy(&output.stderr);
        let stderr = strip_ansi(&stderr_raw);
        let lines = py_splitlines(stderr.trim());
        let stderr = if lines.len() > SHELL_KEEP_TAIL {
            lines[lines.len() - SHELL_KEEP_TAIL..].join("\n")
        } else {
            stderr.trim().to_string()
        };
        return error_response("SHELL_NONZERO", &format!("Exit code {returncode}"), Some(&stderr), None);
    }

    let stdout_raw = String::from_utf8_lossy(&output.stdout);
    let stdout = strip_ansi(&stdout_raw);
    let lines = py_splitlines(stdout.trim());
    // `line_truncated` drives the head/tail formatting; `capture_truncated`
    // (the streaming byte cap was hit) only flips the flag so the response is
    // honest about truncated output even when it still fits in 100 lines.
    let line_truncated = lines.len() > SHELL_MAX_LINES;
    let truncated = line_truncated || capture_truncated;

    let final_output = if line_truncated {
        let head = lines[..SHELL_KEEP_HEAD].join("\n");
        let tail = lines[lines.len() - SHELL_KEEP_TAIL..].join("\n");
        let omitted = lines.len() - SHELL_KEEP_HEAD - SHELL_KEEP_TAIL;
        format!("{head}\n... [{omitted} lines omitted] ...\n{tail}")
    } else {
        stdout.trim().to_string()
    };

    success_response(json!(final_output), None, truncated, Some(json!({"returncode": returncode})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dry_run_does_not_execute() {
        let s = shell("echo should-not-run", true).await;
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["status"], "success");
        assert!(v["message"].as_str().unwrap().contains("DRY RUN"));
    }

    #[tokio::test]
    async fn runs_and_captures_stdout() {
        let s = shell("echo hello-kitty", false).await;
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["status"], "success");
        assert!(v["data"].as_str().unwrap().contains("hello-kitty"));
    }

    #[tokio::test]
    async fn nonzero_exit_reports_shell_nonzero() {
        let s = shell("exit 3", false).await;
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["status"], "error");
        assert_eq!(v["error_code"], "SHELL_NONZERO");
    }

    #[test]
    fn strip_ansi_removes_escape_sequences() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
    }

    #[tokio::test]
    async fn timeout_kills_the_child_process() {
        // A command that would run far longer than the timeout; the timed-out
        // run must reap the direct child, not leak it (regression:
        // `wait_with_output` + dropped future without kill_on_drop left the
        // process running).
        //
        // Note: use a *pure `cmd` builtin* long loop (`for /L ... @rem`) rather
        // than e.g. `ping -n 60` — a toolbox-style command would forklift a
        // grandchild process that survives the parent kill, and a leftover
        // 60s `ping` grandchild delays the test process's own teardown.
        #[cfg(windows)]
        let cmd = "for /L %i in (1,1,2000000000) do @rem";
        #[cfg(not(windows))]
        let cmd = "sleep 60";

        let (response, pid) = shell_impl(cmd, false, Duration::from_millis(300)).await;
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["error_code"], "SHELL_TIMEOUT", "{v}");
        let pid = pid.expect("spawned child must report a pid");

        // Give the kill a moment to land, then assert the process is gone.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!pid_alive(pid), "child process {pid} still alive after timeout");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn timeout_kills_the_whole_process_tree() {
        // Audit #117: `kill_on_drop` reaps only the direct `cmd.exe`; a
        // grandchild it spawned (here `ping`, which `cmd /c` waits on) used
        // to be orphaned and keep running past the timeout. The timeout path
        // now `taskkill /T /F`s the whole tree.
        let before = ping_pids();
        let (response, pid) = shell_impl("ping -n 99999 127.0.0.1 > nul", false, Duration::from_millis(300)).await;
        let v: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["error_code"], "SHELL_TIMEOUT", "{v}");
        assert!(pid.is_some());

        // Give the tree kill a moment to land, then assert no *new* ping
        // survived (pre-existing ones, if any, are not ours to judge).
        tokio::time::sleep(Duration::from_millis(500)).await;
        let leaked: Vec<u32> = ping_pids().into_iter().filter(|p| !before.contains(p)).collect();
        assert!(leaked.is_empty(), "grandchild ping process(es) leaked past the timeout: {leaked:?}");
    }

    #[cfg(windows)]
    fn ping_pids() -> Vec<u32> {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq ping.exe", "/FO", "CSV", "/NH"])
            .output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.split(',').nth(1))
                .filter_map(|f| f.trim_matches('"').parse::<u32>().ok())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    #[tokio::test]
    async fn read_bounded_truncates_oversized_streams() {
        let huge = vec![b'a'; SHELL_MAX_CAPTURE_BYTES + 10_000];
        let (data, truncated) = read_bounded(Some(huge.as_slice()), SHELL_MAX_CAPTURE_BYTES).await;
        assert!(truncated);
        assert!(data.len() <= SHELL_MAX_CAPTURE_BYTES);
        assert!(data.len() >= SHELL_MAX_CAPTURE_BYTES - 8192);
    }

    #[tokio::test]
    async fn read_bounded_passes_through_small_streams() {
        let (data, truncated) = read_bounded(Some(b"hello".as_slice()), SHELL_MAX_CAPTURE_BYTES).await;
        assert_eq!(&data, b"hello");
        assert!(!truncated);
    }

    #[test]
    fn finish_marks_capture_truncation_even_when_under_line_cap() {
        let output = std::process::Output {
            status: success_status(),
            stdout: b"only a few lines\n".to_vec(),
            stderr: Vec::new(),
        };
        let s = finish(output, true);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["truncated"], true);
    }

    #[cfg(windows)]
    fn success_status() -> std::process::ExitStatus {
        std::process::Command::new("cmd").arg("/c").arg("exit 0").status().unwrap()
    }

    #[cfg(not(windows))]
    fn success_status() -> std::process::ExitStatus {
        std::process::Command::new("sh").arg("-c").arg("exit 0").status().unwrap()
    }

    #[cfg(windows)]
    fn pid_alive(pid: u32) -> bool {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}")])
            .output();
        match out {
            Ok(o) => {
                let s = String::from_utf8_lossy(&o.stdout);
                !s.to_lowercase().contains("no tasks") && s.contains(&pid.to_string())
            }
            Err(_) => false,
        }
    }

    #[cfg(not(windows))]
    fn pid_alive(pid: u32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
}
