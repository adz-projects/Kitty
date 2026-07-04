//! Detect a running stock Goose Desktop instance (the Electron app), which can
//! fight this front end over config/sessions/ports. Heuristic and best-effort:
//! false negatives are acceptable; we never block the user, only warn.

use sysinfo::System;

/// Process name of the stock Goose Desktop Electron shell. Matched
/// case-SENSITIVELY so our own lowercase `goose.exe serve` child does not count.
/// (Recorded in docs/VERSIONS.md; re-verify on a Goose version bump.)
const DESKTOP_PROCESS_NAME: &str = "Goose.exe";

/// True if a stock Goose Desktop process is running, excluding our own goosed
/// child pid (belt-and-suspenders alongside the case-sensitive name match).
pub fn goose_desktop_running(exclude_pid: Option<u32>) -> bool {
    let sys = System::new_all();
    for (pid, process) in sys.processes() {
        let name = process.name().to_string_lossy();
        if name == DESKTOP_PROCESS_NAME && Some(pid.as_u32()) != exclude_pid {
            return true;
        }
    }
    false
}
