//! What the overnight window needs from the OS: how long the user has been
//! away, whether the machine is on mains power, and a way to keep it awake
//! (display off) while there is work.
//!
//! Two backends, picked at compile time:
//! * **Windows** — `GetLastInputInfo`, `GetSystemPowerStatus`,
//!   `SetThreadExecutionState`.
//! * **Unix/Linux** — systemd-logind for idle (`loginctl show`'s aggregate
//!   `IdleHint`/`IdleSinceHint`), `/sys/class/power_supply` for mains, and a
//!   held `systemd-inhibit --what=sleep --mode=block` child to stay awake.
//!   On a host without logind/systemd these degrade to "unknown"/no-op, which
//!   [`super::decide`] already treats as "the window still bounds the work".
//!
//! The wire parsing is factored into the platform-agnostic helpers
//! [`idle_from_logind`] and [`ac_from_supplies`] so the logic is unit-tested
//! on every target, not only the one it ships the OS calls for.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Current wall-clock time in microseconds since the Unix epoch — the unit
/// logind reports `IdleSinceHint` in. `0` if the clock is before the epoch
/// (never, in practice), which makes [`idle_from_logind`] report no idle.
fn now_realtime_usec() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Turn logind's aggregate idle report into a duration. `IdleHint` is `yes`
/// (every session idle) or `no` (someone is active); `IdleSinceHint` is the
/// microsecond realtime timestamp the idle began. `no` → zero (active); `yes`
/// → now − since; anything else, or a missing timestamp when idle, → `None`
/// (the caller then counts the host as "away", so the window still bounds it).
fn idle_from_logind(idle_hint: &str, idle_since_usec: Option<u64>, now_usec: u64) -> Option<Duration> {
    match idle_hint.trim() {
        "no" => Some(Duration::ZERO),
        "yes" => Some(Duration::from_micros(now_usec.saturating_sub(idle_since_usec?))),
        _ => None,
    }
}

/// Decide mains-power state from the `/sys/class/power_supply` entries seen.
/// A `Mains`/`USB`/`ADP` supply that is `online` means AC; one present but
/// offline means not on AC. With no mains supply at all (a typical desktop
/// exposes none) the answer is unknown — never guessed from a battery alone,
/// since "no mains entry" does not mean "on battery".
fn ac_from_supplies(saw_mains: bool, mains_online: bool) -> Option<bool> {
    if saw_mains {
        Some(mains_online)
    } else {
        None
    }
}

/// Time since the last keyboard or mouse input in this session, or `None`
/// when the OS cannot say.
pub fn idle_for() -> Option<Duration> {
    imp::idle_for()
}

/// `Some(true)` on mains power, `Some(false)` on battery, `None` unknown (a
/// desktop with no battery reports mains).
pub fn on_ac_power() -> Option<bool> {
    imp::on_ac_power()
}

/// Keeps the system awake (never the display) while a lease is live. Each
/// [`KeepAwake::extend`] pushes the lease out; a helper thread holds the
/// request while the lease is live and drops it once it lapses, after which a
/// timer wake's normal unattended-sleep timeout puts the machine back to
/// sleep. A thread of its own because the request belongs to the thread that
/// made it, and async tasks move between threads.
pub struct KeepAwake {
    until: Arc<Mutex<Option<Instant>>>,
    started: std::sync::Once,
}

impl Default for KeepAwake {
    fn default() -> Self {
        Self {
            until: Arc::new(Mutex::new(None)),
            started: std::sync::Once::new(),
        }
    }
}

/// How often the helper thread re-checks the lease.
const LEASE_POLL: Duration = Duration::from_secs(5);

impl KeepAwake {
    pub fn extend(&self, by: Duration) {
        let target = Instant::now() + by;
        {
            let mut until = self.until.lock().unwrap_or_else(|e| e.into_inner());
            if until.is_none_or(|u| u < target) {
                *until = Some(target);
            }
        }
        self.started.call_once(|| {
            let until = self.until.clone();
            let spawned = std::thread::Builder::new()
                .name("overnight-keep-awake".into())
                .spawn(move || {
                    let mut held = false;
                    loop {
                        let live = until
                            .lock()
                            .map(|u| u.is_some_and(|t| t > Instant::now()))
                            .unwrap_or(false);
                        if live != held {
                            imp::hold_awake(live);
                            held = live;
                        }
                        std::thread::sleep(LEASE_POLL);
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!("overnight: keep-awake thread failed to start: {e}");
            }
        });
    }

    /// Whether a lease is live (diagnostics, tests).
    pub fn is_live(&self) -> bool {
        self.until
            .lock()
            .map(|u| u.is_some_and(|t| t > Instant::now()))
            .unwrap_or(false)
    }
}

#[cfg(windows)]
mod imp {
    use std::time::Duration;
    use windows_sys::Win32::System::Power::{
        GetSystemPowerStatus, SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED,
        SYSTEM_POWER_STATUS,
    };
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    pub fn idle_for() -> Option<Duration> {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        // SAFETY: `info` is a properly sized, initialised LASTINPUTINFO.
        let ok = unsafe { GetLastInputInfo(&mut info) };
        if ok == 0 {
            return None;
        }
        // Both are 32-bit millisecond tick counts; wrapping_sub stays correct
        // across the 49.7-day wrap.
        // SAFETY: no arguments, no preconditions.
        let now = unsafe { GetTickCount() };
        Some(Duration::from_millis(now.wrapping_sub(info.dwTime) as u64))
    }

    pub fn on_ac_power() -> Option<bool> {
        // SAFETY: zeroed is a valid SYSTEM_POWER_STATUS; the call fills it.
        let mut s: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
        // SAFETY: `s` is a valid out-pointer.
        if unsafe { GetSystemPowerStatus(&mut s) } == 0 {
            return None;
        }
        match s.ACLineStatus {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    pub fn hold_awake(on: bool) {
        let flags = if on {
            ES_CONTINUOUS | ES_SYSTEM_REQUIRED
        } else {
            ES_CONTINUOUS
        };
        // SAFETY: plain flags; affects only the calling thread's request.
        unsafe { SetThreadExecutionState(flags) };
    }
}

#[cfg(not(windows))]
mod imp {
    use std::process::{Child, Command, Stdio};
    use std::sync::Mutex;
    use std::time::Duration;

    /// Idle time from systemd-logind's Manager object, via `loginctl show`.
    /// The Manager's `IdleHint`/`IdleSinceHint` aggregate every seat/session,
    /// so this is "has the whole machine been idle", which is what the window
    /// wants. `None` when logind is absent or the call fails — on a headless
    /// server there is no graphical idle, and "unknown" counts as away.
    pub fn idle_for() -> Option<Duration> {
        let out = Command::new("loginctl")
            .args(["show", "--property=IdleHint", "--property=IdleSinceHint"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut hint: Option<String> = None;
        let mut since: Option<u64> = None;
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("IdleHint=") {
                hint = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("IdleSinceHint=") {
                since = v.trim().parse::<u64>().ok();
            }
        }
        super::idle_from_logind(hint.as_deref()?, since, super::now_realtime_usec())
    }

    /// Mains state from `/sys/class/power_supply`. A desktop with no battery
    /// typically exposes no supply directory at all → `None` (unknown), which
    /// the catch-up rule treats as "not on battery".
    pub fn on_ac_power() -> Option<bool> {
        let dir = std::fs::read_dir("/sys/class/power_supply").ok()?;
        let mut saw_mains = false;
        let mut mains_online = false;
        for entry in dir.flatten() {
            let path = entry.path();
            let kind = std::fs::read_to_string(path.join("type")).unwrap_or_default();
            // Mains (desktop/ATX), ADP (laptop adapter) and USB-PD all count as
            // "plugged in".
            if matches!(kind.trim(), "Mains" | "ADP" | "USB") {
                saw_mains = true;
                if std::fs::read_to_string(path.join("online"))
                    .map(|s| s.trim() == "1")
                    .unwrap_or(false)
                {
                    mains_online = true;
                }
            }
        }
        super::ac_from_supplies(saw_mains, mains_online)
    }

    /// The held `systemd-inhibit` child that keeps the system awake while a
    /// pass runs. `systemd-inhibit CMD` holds the lock only while `CMD` runs,
    /// so we keep a `sleep infinity` child alive and kill it to release.
    static INHIBITOR: Mutex<Option<Child>> = Mutex::new(None);

    pub fn hold_awake(on: bool) {
        let mut held = INHIBITOR.lock().unwrap_or_else(|e| e.into_inner());
        if on {
            if held.is_none() {
                match Command::new("systemd-inhibit")
                    .args([
                        "--what=sleep",
                        "--who=Kitty",
                        "--why=overnight memory learning",
                        "--mode=block",
                        "sleep",
                        "infinity",
                    ])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    Ok(child) => *held = Some(child),
                    Err(e) => {
                        tracing::warn!("overnight: systemd-inhibit unavailable, cannot hold the machine awake: {e}");
                    }
                }
            }
        } else if let Some(mut child) = held.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lease_is_live_until_it_lapses() {
        let k = KeepAwake::default();
        assert!(!k.is_live());
        k.extend(Duration::from_secs(60));
        assert!(k.is_live());
        // A shorter extension never shortens a live lease.
        k.extend(Duration::from_millis(1));
        assert!(k.is_live());
    }

    #[test]
    fn logind_idle_parsing() {
        let now = 1_000_000_000u64; // 1000 s in usec
        // Active session → zero idle regardless of the (stale) since stamp.
        assert_eq!(idle_from_logind("no", Some(1), now), Some(Duration::ZERO));
        // Idle since 400 s → 600 s idle.
        assert_eq!(
            idle_from_logind("yes", Some(400_000_000), now),
            Some(Duration::from_secs(600))
        );
        // Idle but no timestamp, or an unknown hint → unknown.
        assert_eq!(idle_from_logind("yes", None, now), None);
        assert_eq!(idle_from_logind("", Some(1), now), None);
        // A since stamp in the future never yields a negative duration.
        assert_eq!(idle_from_logind("yes", Some(now + 5), now), Some(Duration::ZERO));
    }

    #[test]
    fn mains_state_from_supplies() {
        assert_eq!(ac_from_supplies(true, true), Some(true), "mains online → on AC");
        assert_eq!(ac_from_supplies(true, false), Some(false), "mains present but offline → on battery");
        assert_eq!(ac_from_supplies(false, false), None, "no mains supply → unknown, never guessed");
    }
}
