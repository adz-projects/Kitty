//! Waking the computer for overnight learning (docs/MEMORY_OVERNIGHT.md).
//!
//! A per-user Windows scheduled task, `Kitty\Overnight memory`, runs
//! `kitty.exe --overnight-wake` daily a few minutes after the window opens,
//! with *Wake the computer to run this task* set and only on mains power. The
//! process exits at once ([`requested`] is checked at the top of `run()`); the
//! wake itself is the point, and the engine's own clock does the work. When
//! the work is done the machine goes back to sleep on Windows' own
//! unattended-wake timeout.
//!
//! Also reports whether wake timers are allowed in the active power plan,
//! which Settings shows rather than changes: that is a system setting.

// The task itself is Windows-only; Android compiles the types the commands
// share and nothing else uses them.
#![cfg_attr(target_os = "android", allow(dead_code))]

use serde::Serialize;

pub const FLAG: &str = "--overnight-wake";

/// The task's name in Task Scheduler (a folder of Kitty's own).
pub const TASK_NAME: &str = "Kitty\\Overnight memory";

/// Minutes after the window opens that the task wakes the machine.
pub const WAKE_AFTER_START_MIN: u32 = 5;

/// `kitty.exe --overnight-wake`: nothing to do but exit.
pub fn requested() -> bool {
    std::env::args().skip(1).any(|a| a == FLAG)
}

/// Whether the active power plan lets timers wake the machine, on mains and
/// on battery. `None` when it could not be read.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct WakeTimers {
    pub ac: Option<bool>,
    pub dc: Option<bool>,
}

/// The time the task fires: the window's start plus a few minutes, `HH:MM`.
pub fn wake_time(start: &str) -> Option<String> {
    let (h, m) = start.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    if h > 23 || m > 59 {
        return None;
    }
    let total = (h * 60 + m + WAKE_AFTER_START_MIN) % (24 * 60);
    Some(format!("{:02}:{:02}", total / 60, total % 60))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The task definition. Pure, so it is testable.
pub fn task_xml(exe: &str, at: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Wakes this computer so Kitty's memory can learn overnight. Turn it off in Kitty: Settings, Memorabilia, Overnight learning.</Description>
  </RegistrationInfo>
  <Triggers>
    <CalendarTrigger>
      <StartBoundary>2026-01-01T{at}:00</StartBoundary>
      <Enabled>true</Enabled>
      <ScheduleByDay>
        <DaysInterval>1</DaysInterval>
      </ScheduleByDay>
    </CalendarTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>true</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>true</WakeToRun>
    <ExecutionTimeLimit>PT5M</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>{flag}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        exe = xml_escape(exe),
        at = at,
        flag = FLAG,
    )
}

/// Parse `powercfg /q SCHEME_CURRENT SUB_SLEEP RTCWAKE`: index 0 is
/// "disable", 1 "enable", 2 "important wake timers only" (which excludes a
/// scheduled task's).
pub fn parse_powercfg(out: &str) -> WakeTimers {
    let value = |l: &str| {
        l.rsplit_once("0x")
            .and_then(|(_, hex)| u32::from_str_radix(hex.trim(), 16).ok())
    };
    let read = |label: &str| out.lines().find(|l| l.contains(label)).and_then(value);
    let (mut ac, mut dc) = (
        read("Current AC Power Setting Index"),
        read("Current DC Power Setting Index"),
    );
    // A localized Windows words the labels differently; the current AC and
    // DC indexes are still the last two hex values printed.
    if ac.is_none() && dc.is_none() {
        let hex: Vec<u32> = out.lines().filter_map(value).collect();
        if hex.len() >= 2 {
            ac = Some(hex[hex.len() - 2]);
            dc = Some(hex[hex.len() - 1]);
        }
    }
    WakeTimers {
        ac: ac.map(|v| v == 1),
        dc: dc.map(|v| v == 1),
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::util::hidden_command;
    use std::path::Path;

    fn system32(exe: &str) -> std::path::PathBuf {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        Path::new(&root).join("System32").join(exe)
    }

    pub fn is_registered() -> bool {
        hidden_command(&system32("schtasks.exe"))
            .args(["/Query", "/TN", TASK_NAME])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    pub fn register(start: &str) -> Result<(), String> {
        let at = wake_time(start).ok_or_else(|| format!("'{start}' is not a time"))?;
        let exe = std::env::current_exe()
            .map_err(|e| format!("could not find Kitty's own path: {e}"))?;
        let xml = task_xml(&exe.to_string_lossy(), &at);
        // Task Scheduler reads task XML as UTF-16 with a byte-order mark.
        let mut bytes: Vec<u8> = vec![0xFF, 0xFE];
        for u in xml.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let path = std::env::temp_dir().join("kitty-overnight-task.xml");
        std::fs::write(&path, bytes).map_err(|e| format!("could not write the task file: {e}"))?;
        let out = hidden_command(&system32("schtasks.exe"))
            .args(["/Create", "/F", "/TN", TASK_NAME, "/XML"])
            .arg(&path)
            .output()
            .map_err(|e| format!("could not run schtasks: {e}"));
        let _ = std::fs::remove_file(&path);
        let out = out?;
        if out.status.success() {
            Ok(())
        } else {
            let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
            tracing::warn!("schtasks /Create failed: {detail}");
            Err("Windows did not accept the wake-up task.".into())
        }
    }

    pub fn unregister() -> Result<(), String> {
        if !is_registered() {
            return Ok(());
        }
        let out = hidden_command(&system32("schtasks.exe"))
            .args(["/Delete", "/F", "/TN", TASK_NAME])
            .output()
            .map_err(|e| format!("could not run schtasks: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            tracing::warn!(
                "schtasks /Delete failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            Err("Windows did not remove the wake-up task.".into())
        }
    }

    pub fn wake_timers() -> WakeTimers {
        match hidden_command(&system32("powercfg.exe"))
            .args(["/q", "SCHEME_CURRENT", "SUB_SLEEP", "RTCWAKE"])
            .output()
        {
            Ok(o) if o.status.success() => parse_powercfg(&String::from_utf8_lossy(&o.stdout)),
            _ => WakeTimers::default(),
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub fn is_registered() -> bool {
        false
    }
    pub fn register(_start: &str) -> Result<(), String> {
        Err("Waking the computer is only available on Windows.".into())
    }
    pub fn unregister() -> Result<(), String> {
        Ok(())
    }
    pub fn wake_timers() -> WakeTimers {
        WakeTimers::default()
    }
}

pub use imp::{is_registered, register, unregister, wake_timers};

/// Make the task match the settings: registered (at the window's start) when
/// overnight learning and waking are both on, removed otherwise.
pub fn sync(settings: &crate::config::OvernightSettings) -> Result<(), String> {
    if settings.enabled && settings.wake {
        register(&settings.start)
    } else {
        unregister()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_task_fires_shortly_after_the_window_opens() {
        assert_eq!(wake_time("00:00").as_deref(), Some("00:05"));
        assert_eq!(wake_time("23:58").as_deref(), Some("00:03"));
        assert_eq!(wake_time("25:00"), None);
        assert_eq!(wake_time("noon"), None);
    }

    #[test]
    fn the_task_wakes_on_mains_and_runs_the_wake_flag() {
        let x = task_xml(r"C:\Program Files\Kitty & Co\kitty.exe", "00:05");
        assert!(x.contains("<WakeToRun>true</WakeToRun>"));
        assert!(x.contains("<DisallowStartIfOnBatteries>true</DisallowStartIfOnBatteries>"));
        assert!(x.contains("T00:05:00</StartBoundary>"));
        assert!(x.contains("Kitty &amp; Co"));
        assert!(x.contains("<Arguments>--overnight-wake</Arguments>"));
    }

    #[test]
    fn wake_timer_settings_are_read_from_powercfg() {
        let out = "    Possible Setting Index: 002\n    Current AC Power Setting Index: 0x00000001\n    Current DC Power Setting Index: 0x00000000\n";
        assert_eq!(parse_powercfg(out), WakeTimers { ac: Some(true), dc: Some(false) });
        assert_eq!(parse_powercfg(""), WakeTimers::default());
        let important_only = "Current AC Power Setting Index: 0x00000002";
        assert_eq!(parse_powercfg(important_only).ac, Some(false));
        // A localized Windows: the labels differ, the last two values don't.
        let fr = "    Index du paramètre CA actuel : 0x00000000\n    Index du paramètre CC actuel : 0x00000001\n";
        assert_eq!(parse_powercfg(fr), WakeTimers { ac: Some(false), dc: Some(true) });
    }
}
