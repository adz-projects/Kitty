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

/// Check whether an existing task's XML already matches the desired executable
/// path, wake time, and wake-to-run flags so startup skips redundant writes.
pub fn xml_matches(existing_xml: &str, exe: &str, at: &str) -> bool {
    let escaped_exe = xml_escape(exe);
    existing_xml.contains(&format!("<Command>{escaped_exe}</Command>"))
        && existing_xml.contains(&format!("<Arguments>{FLAG}</Arguments>"))
        && existing_xml.contains(&format!("T{at}:00</StartBoundary>"))
        && existing_xml.contains("<WakeToRun>true</WakeToRun>")
        && existing_xml.contains("<Enabled>true</Enabled>")
}

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::util::hidden_command;
    use std::path::Path;
    use windows::core::{BSTR, VARIANT};
    use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, VARIANT_TRUE};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::System::TaskScheduler::{
        IRegisteredTask, ITaskFolder, ITaskService, TaskScheduler, TASK_CREATE_OR_UPDATE,
        TASK_LOGON_INTERACTIVE_TOKEN,
    };

    const FOLDER_PATH: &str = "\\Kitty";
    const FOLDER_NAME: &str = "Kitty";
    const TASK_SUBNAME: &str = "Overnight memory";

    struct ComGuard(bool);

    impl ComGuard {
        fn init() -> Result<Self, String> {
            let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if hr.is_ok() {
                Ok(Self(true))
            } else if hr == RPC_E_CHANGED_MODE {
                Ok(Self(false))
            } else {
                Err(format!("COM init failed: {}", hr.message()))
            }
        }
    }

    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    fn connect() -> Result<(ComGuard, ITaskService), String> {
        let guard = ComGuard::init()?;
        let service: ITaskService =
            unsafe { CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER) }
                .map_err(|e| format!("could not open Task Scheduler: {e}"))?;
        let empty = VARIANT::default();
        unsafe { service.Connect(&empty, &empty, &empty, &empty) }
            .map_err(|e| format!("could not connect to Task Scheduler: {e}"))?;
        Ok((guard, service))
    }

    fn open_folder(service: &ITaskService) -> Option<ITaskFolder> {
        unsafe { service.GetFolder(&BSTR::from(FOLDER_PATH)) }.ok()
    }

    fn open_task(folder: &ITaskFolder) -> Option<IRegisteredTask> {
        unsafe { folder.GetTask(&BSTR::from(TASK_SUBNAME)) }.ok()
    }

    fn ensure_folder(service: &ITaskService) -> Result<ITaskFolder, String> {
        if let Some(folder) = open_folder(service) {
            return Ok(folder);
        }
        let root = unsafe { service.GetFolder(&BSTR::from("\\")) }
            .map_err(|e| format!("could not open root Task Scheduler folder: {e}"))?;
        let empty = VARIANT::default();
        unsafe { root.CreateFolder(&BSTR::from(FOLDER_NAME), &empty) }
            .or_else(|e| open_folder(service).ok_or_else(|| format!("could not create task folder: {e}")))
    }

    fn task_already_matches(task: &IRegisteredTask, exe: &str, at: &str) -> bool {
        let enabled = unsafe { task.Enabled() }.ok() == Some(VARIANT_TRUE);
        if !enabled {
            return false;
        }
        let Ok(xml) = (unsafe { task.Xml() }) else {
            return false;
        };
        xml_matches(&xml.to_string(), exe, at)
    }

    fn system32(exe: &str) -> std::path::PathBuf {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        Path::new(&root).join("System32").join(exe)
    }

    pub fn is_registered() -> bool {
        let Ok((_guard, service)) = connect() else {
            return false;
        };
        open_folder(&service).is_some_and(|f| open_task(&f).is_some())
    }

    pub fn register(start: &str) -> Result<(), String> {
        let at = wake_time(start).ok_or_else(|| format!("'{start}' is not a time"))?;
        let exe =
            std::env::current_exe().map_err(|e| format!("could not find Kitty's own path: {e}"))?;
        let exe_str = exe.to_string_lossy();
        let (_guard, service) = connect()?;

        if let Some(folder) = open_folder(&service) {
            if let Some(existing) = open_task(&folder) {
                if task_already_matches(&existing, &exe_str, &at) {
                    return Ok(());
                }
            }
        }

        let folder = ensure_folder(&service)?;
        let xml = task_xml(&exe_str, &at);
        let empty = VARIANT::default();
        unsafe {
            folder.RegisterTask(
                &BSTR::from(TASK_SUBNAME),
                &BSTR::from(xml),
                TASK_CREATE_OR_UPDATE.0,
                &empty,
                &empty,
                TASK_LOGON_INTERACTIVE_TOKEN,
                &empty,
            )
        }
        .map_err(|e| {
            tracing::warn!("Task Scheduler RegisterTask failed: {e}");
            "Windows did not accept the wake-up task.".to_string()
        })?;
        Ok(())
    }

    pub fn unregister() -> Result<(), String> {
        let (_guard, service) = connect()?;
        let Some(folder) = open_folder(&service) else {
            return Ok(());
        };
        if open_task(&folder).is_none() {
            return Ok(());
        }
        unsafe { folder.DeleteTask(&BSTR::from(TASK_SUBNAME), 0) }.map_err(|e| {
            tracing::warn!("Task Scheduler DeleteTask failed: {e}");
            "Windows did not remove the wake-up task.".to_string()
        })
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
        assert!(xml_matches(&x, r"C:\Program Files\Kitty & Co\kitty.exe", "00:05"));
        assert!(!xml_matches(&x, r"C:\Users\adamz\AppData\Local\Kitty\kitty.exe", "00:05"));
        assert!(!xml_matches(&x, r"C:\Program Files\Kitty & Co\kitty.exe", "01:05"));
        assert!(!xml_matches(
            &x.replace("<WakeToRun>true</WakeToRun>", "<WakeToRun>false</WakeToRun>"),
            r"C:\Program Files\Kitty & Co\kitty.exe",
            "00:05",
        ));
    }

    #[test]
    fn wake_timer_settings_are_read_from_powercfg() {
        let out = "    Possible Setting Index: 002\n    Current AC Power Setting Index: 0x00000001\n    Current DC Power Setting Index: 0x00000000\n";
        assert_eq!(
            parse_powercfg(out),
            WakeTimers {
                ac: Some(true),
                dc: Some(false)
            }
        );
        assert_eq!(parse_powercfg(""), WakeTimers::default());
        let important_only = "Current AC Power Setting Index: 0x00000002";
        assert_eq!(parse_powercfg(important_only).ac, Some(false));
        // A localized Windows: the labels differ, the last two values don't.
        let fr = "    Index du paramètre CA actuel : 0x00000000\n    Index du paramètre CC actuel : 0x00000001\n";
        assert_eq!(
            parse_powercfg(fr),
            WakeTimers {
                ac: Some(false),
                dc: Some(true)
            }
        );
    }
}
