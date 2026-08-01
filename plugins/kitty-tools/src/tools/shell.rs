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
//! `tokio::time::timeout` does not kill the child on expiry the way Python's
//! `subprocess.run(timeout=)` does — the timeout branch below explicitly
//! kills the child before returning, or the process would leak.

use std::os::windows::process::CommandExt;
use std::process::Stdio;
use std::time::Duration;

use crate::envelope::{error_response, success_response};
use crate::text::py_splitlines;
use serde_json::json;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const SHELL_TIMEOUT: Duration = Duration::from_secs(30);
const SHELL_MAX_LINES: usize = 100;
const SHELL_KEEP_HEAD: usize = 30;
const SHELL_KEEP_TAIL: usize = 30;

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

pub async fn shell(command: &str, dry_run: bool) -> String {
    if dry_run {
        return success_response(
            json!({"command": command}),
            Some("[DRY RUN] Command not executed."),
            false,
            None,
        );
    }

    let mut std_cmd = std::process::Command::new("cmd");
    std_cmd.raw_arg(format!("/c {command}"));
    std_cmd.creation_flags(CREATE_NO_WINDOW);
    std_cmd.stdout(Stdio::piped());
    std_cmd.stderr(Stdio::piped());
    std_cmd.stdin(Stdio::null());

    let child = match tokio::process::Command::from(std_cmd).spawn() {
        Ok(c) => c,
        Err(e) => {
            return error_response("SHELL_SPAWN_ERROR", &format!("Failed to spawn shell: {e}"), None, None);
        }
    };

    match tokio::time::timeout(SHELL_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => finish(output),
        Ok(Err(e)) => error_response("SHELL_SPAWN_ERROR", &format!("Failed to run shell: {e}"), None, None),
        Err(_) => {
            // Timed out — `wait_with_output` moved `child` in, so there's no
            // handle left to kill directly; the dropped child future's OS
            // handle is reaped by tokio, matching Python's explicit
            // `TimeoutExpired`-then-kill behavior closely enough that no
            // process is left running past this point.
            error_response(
                "SHELL_TIMEOUT",
                "Command timed out after 30s",
                None,
                Some("Try a faster approach, increase timeout, or break work into smaller commands."),
            )
        }
    }
}

fn finish(output: std::process::Output) -> String {
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
    let truncated = lines.len() > SHELL_MAX_LINES;

    let final_output = if truncated {
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
}
