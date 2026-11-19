//! A ceiling on the daemon's own memory, and a log of how it grows.
//!
//! On 2026-11-18 a daemon committed ~53 GB on a 16 GB machine and starved the
//! whole system before it was killed, and nothing in its log said why. Two
//! defences, both Windows-only (the Android host lives inside the app process,
//! whose memory the OS already polices):
//!
//! - [`install_ceiling`] puts the daemon in a job object with a per-process
//!   commit limit. Past it allocations fail, and Rust aborts with "memory
//!   allocation of N bytes failed" in `daemon.log`. Losing the engine — which
//!   clients already know how to respawn — is far better than losing the
//!   machine.
//! - [`spawn_watchdog`] logs a warning each time private memory crosses a new
//!   step, so the log shows when growth began and how fast it ran.
//!
//! `BIGTINYV2_MEMORY_LIMIT_MB` overrides the ceiling; `0` turns it off.

/// Default ceiling as a share of physical RAM.
const DEFAULT_SHARE_OF_RAM: f64 = 0.5;
/// Never cap below this, whatever the machine: the summarizer model alone
/// needs a few GB.
const MIN_CEILING_BYTES: u64 = 4 << 30;
/// First private-memory level the watchdog reports; each later report is at
/// twice the previous one.
const FIRST_REPORT_BYTES: u64 = 2 << 30;
const SAMPLE_EVERY: std::time::Duration = std::time::Duration::from_secs(15);

/// The ceiling to apply, or `None` when it is turned off.
fn ceiling_bytes(env: Option<&str>, physical: u64) -> Option<u64> {
    if let Some(v) = env {
        if let Ok(mb) = v.trim().parse::<u64>() {
            return (mb > 0).then_some(mb << 20);
        }
        // An unparseable value falls back to the default rather than
        // disabling the guard: failing open on a safety limit is how the
        // machine gets eaten.
    }
    let share = (physical as f64 * DEFAULT_SHARE_OF_RAM) as u64;
    Some(share.max(MIN_CEILING_BYTES))
}

/// The next level the watchdog should report at, given the current usage
/// and the last level it reported.
fn next_report_level(private: u64, last: u64) -> Option<u64> {
    let mut level = last.max(FIRST_REPORT_BYTES / 2) * 2;
    if private < level {
        return None;
    }
    while private >= level * 2 {
        level *= 2;
    }
    Some(level)
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / f64::from(1u32 << 30)
}

#[cfg(windows)]
mod sys {
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    pub fn physical_memory() -> Option<u64> {
        // SAFETY: a zeroed MEMORYSTATUSEX with `dwLength` set is the
        // documented input; the call only writes into it.
        unsafe {
            let mut status: MEMORYSTATUSEX = std::mem::zeroed();
            status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            (GlobalMemoryStatusEx(&mut status) != 0).then_some(status.ullTotalPhys)
        }
    }

    pub fn private_bytes() -> Option<u64> {
        // SAFETY: the _EX struct starts with the base struct's fields, and
        // `cb` tells the call which one it was given.
        unsafe {
            let mut counters: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
            counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
            let ok = K32GetProcessMemoryInfo(
                GetCurrentProcess(),
                (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
                counters.cb,
            );
            (ok != 0).then_some(counters.PrivateUsage as u64)
        }
    }

    /// Put this process in a fresh job with a per-process commit limit.
    ///
    /// The job handle is deliberately leaked: closing it would not lift the
    /// limit, but keeping it open for the process's life is the documented
    /// way to hold a job. No kill-on-close flag is set, so the MCP servers the
    /// daemon starts (which inherit the job, and get the same per-process
    /// limit) are never taken down with it.
    pub fn limit_process_memory(bytes: u64) -> Result<(), String> {
        // SAFETY: plain Win32 calls on handles we own; the info struct is
        // fully initialised before it is passed.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(format!("CreateJobObjectW: {}", std::io::Error::last_os_error()));
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            info.ProcessMemoryLimit = bytes as usize;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                return Err(format!(
                    "SetInformationJobObject: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
                return Err(format!(
                    "AssignProcessToJobObject: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(())
        }
    }
}

/// Cap this process's committed memory. Call once, early, after logging is
/// set up. Never fails the daemon: a ceiling that cannot be applied is
/// logged and the daemon runs without one.
#[cfg(windows)]
pub fn install_ceiling() {
    let env = std::env::var("BIGTINYV2_MEMORY_LIMIT_MB").ok();
    let Some(physical) = sys::physical_memory() else {
        tracing::warn!("memory ceiling not applied: physical memory size unknown");
        return;
    };
    let Some(limit) = ceiling_bytes(env.as_deref(), physical) else {
        tracing::warn!("memory ceiling off (BIGTINYV2_MEMORY_LIMIT_MB=0)");
        return;
    };
    match sys::limit_process_memory(limit) {
        Ok(()) => tracing::info!(
            "memory ceiling {:.1} GiB (of {:.1} GiB physical)",
            gib(limit),
            gib(physical)
        ),
        Err(e) => tracing::warn!("memory ceiling not applied: {e}"),
    }
}

#[cfg(not(windows))]
pub fn install_ceiling() {}

/// Log each time private memory reaches a new doubling step (2, 4, 8 GiB …).
#[cfg(windows)]
pub fn spawn_watchdog() {
    tokio::spawn(async move {
        let mut reported = 0u64;
        let mut tick = tokio::time::interval(SAMPLE_EVERY);
        loop {
            tick.tick().await;
            let Some(private) = sys::private_bytes() else {
                continue;
            };
            if let Some(level) = next_report_level(private, reported) {
                reported = level;
                tracing::warn!(
                    "daemon private memory is {:.1} GiB (passed {:.0} GiB)",
                    gib(private),
                    gib(level)
                );
            }
        }
    });
}

#[cfg(not(windows))]
pub fn spawn_watchdog() {}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn the_default_ceiling_is_half_of_ram_but_never_below_the_floor() {
        assert_eq!(ceiling_bytes(None, 16 * GIB), Some(8 * GIB));
        assert_eq!(ceiling_bytes(None, 4 * GIB), Some(MIN_CEILING_BYTES));
    }

    #[test]
    fn the_env_override_sets_or_disables_the_ceiling() {
        assert_eq!(ceiling_bytes(Some("2048"), 16 * GIB), Some(2 * GIB));
        assert_eq!(ceiling_bytes(Some("0"), 16 * GIB), None);
        // Garbage keeps the guard on.
        assert_eq!(ceiling_bytes(Some("lots"), 16 * GIB), Some(8 * GIB));
    }

    #[test]
    fn the_watchdog_reports_each_doubling_once() {
        assert_eq!(next_report_level(GIB, 0), None);
        assert_eq!(next_report_level(2 * GIB, 0), Some(2 * GIB));
        assert_eq!(next_report_level(3 * GIB, 2 * GIB), None);
        assert_eq!(next_report_level(5 * GIB, 2 * GIB), Some(4 * GIB));
        // A jump straight past several steps reports the highest one passed.
        assert_eq!(next_report_level(40 * GIB, 2 * GIB), Some(32 * GIB));
    }
}
