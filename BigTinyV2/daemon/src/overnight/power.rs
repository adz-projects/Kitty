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
use std::time::{Duration, Instant};
#[cfg(not(windows))]
use std::time::{SystemTime, UNIX_EPOCH};

/// Current wall-clock time in microseconds since the Unix epoch — the unit
/// logind reports `IdleSinceHint` in. `0` if the clock is before the epoch
/// (never, in practice), which makes [`idle_from_logind`] report no idle.
#[cfg(not(windows))]
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
#[cfg(any(not(windows), test))]
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
#[cfg(any(not(windows), test))]
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

/// CPU load (all cores) at or above which the machine counts as busy.
pub const BUSY_CPU: f64 = 0.50;
/// GPU 3D-engine load at or above which the machine counts as busy.
pub const BUSY_GPU: f64 = 0.40;

/// CPU and GPU utilisation, each `0.0..=1.0`, averaged over `window`
/// (blocking: the caller sleeps through it). `None` for a figure the OS
/// cannot give; GPU load is read on Windows only.
pub fn load_sample(window: Duration) -> (Option<f64>, Option<f64>) {
    imp::load_sample(window)
}

/// Whether something else is using the machine heavily (a build, render,
/// download or game running while the user is away). An unknown figure
/// never counts as busy.
pub fn is_busy(cpu: Option<f64>, gpu: Option<f64>) -> bool {
    cpu.is_some_and(|c| c >= BUSY_CPU) || gpu.is_some_and(|g| g >= BUSY_GPU)
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

    fn filetime(ft: &windows_sys::Win32::Foundation::FILETIME) -> u64 {
        ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
    }

    /// `(idle, kernel, user)` system times; kernel includes idle.
    fn system_times() -> Option<(u64, u64, u64)> {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::GetSystemTimes;
        // SAFETY: zeroed FILETIMEs are valid out-parameters.
        let (mut i, mut k, mut u): (FILETIME, FILETIME, FILETIME) =
            unsafe { (std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed()) };
        // SAFETY: three valid out-pointers.
        if unsafe { GetSystemTimes(&mut i, &mut k, &mut u) } == 0 {
            return None;
        }
        Some((filetime(&i), filetime(&k), filetime(&u)))
    }

    /// A PDH query over every process's 3D-engine utilisation.
    struct GpuQuery {
        query: windows_sys::Win32::System::Performance::PDH_HQUERY,
        counter: windows_sys::Win32::System::Performance::PDH_HCOUNTER,
    }

    impl GpuQuery {
        fn open() -> Option<Self> {
            use windows_sys::Win32::System::Performance::{
                PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhOpenQueryW,
            };
            let mut query = std::ptr::null_mut();
            // SAFETY: null data source = live counters; valid out-pointer.
            if unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut query) } != 0 {
                return None;
            }
            let path: Vec<u16> = "\\GPU Engine(*engtype_3D)\\Utilization Percentage"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let mut counter = std::ptr::null_mut();
            // SAFETY: `query` is open; `path` is NUL-terminated UTF-16.
            if unsafe { PdhAddEnglishCounterW(query, path.as_ptr(), 0, &mut counter) } != 0 {
                // SAFETY: `query` is open.
                unsafe { PdhCloseQuery(query) };
                return None;
            }
            // First collection: a rate counter needs two.
            // SAFETY: `query` is open.
            unsafe { PdhCollectQueryData(query) };
            Some(Self { query, counter })
        }

        /// Total 3D utilisation since `open`, `0.0..=1.0`.
        fn read(&self) -> Option<f64> {
            use windows_sys::Win32::System::Performance::{
                PdhCollectQueryData, PdhGetFormattedCounterArrayW, PDH_FMT_COUNTERVALUE_ITEM_W,
                PDH_FMT_DOUBLE, PDH_MORE_DATA,
            };
            // SAFETY: `query` is open.
            if unsafe { PdhCollectQueryData(self.query) } != 0 {
                return None;
            }
            let (mut bytes, mut count) = (0u32, 0u32);
            // SAFETY: a null buffer with size 0 asks for the size needed.
            let status = unsafe {
                PdhGetFormattedCounterArrayW(
                    self.counter,
                    PDH_FMT_DOUBLE,
                    &mut bytes,
                    &mut count,
                    std::ptr::null_mut(),
                )
            };
            if status != PDH_MORE_DATA || bytes == 0 {
                return if count == 0 { Some(0.0) } else { None };
            }
            // 8-byte aligned storage for the items plus their name strings.
            let mut buf = vec![0u64; (bytes as usize).div_ceil(8)];
            let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            // SAFETY: `buf` holds `bytes` bytes, aligned for the item type.
            let status = unsafe {
                PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut bytes, &mut count, items)
            };
            if status != 0 {
                return None;
            }
            let mut total = 0.0f64;
            for n in 0..count as usize {
                // SAFETY: PDH wrote `count` items at the start of `buf`.
                let item = unsafe { &*items.add(n) };
                // CStatus 0 (valid) or 1 (new data) carry a value.
                if item.FmtValue.CStatus <= 1 {
                    // SAFETY: PDH_FMT_DOUBLE fills `doubleValue`.
                    total += unsafe { item.FmtValue.Anonymous.doubleValue };
                }
            }
            Some((total / 100.0).clamp(0.0, 1.0))
        }
    }

    impl Drop for GpuQuery {
        fn drop(&mut self) {
            // SAFETY: `query` is open and closed once, here.
            unsafe { windows_sys::Win32::System::Performance::PdhCloseQuery(self.query) };
        }
    }

    pub fn load_sample(window: Duration) -> (Option<f64>, Option<f64>) {
        let t0 = system_times();
        let gpu = GpuQuery::open();
        std::thread::sleep(window);
        let t1 = system_times();
        let gpu = gpu.and_then(|g| g.read());
        let cpu = match (t0, t1) {
            (Some((i0, k0, u0)), Some((i1, k1, u1))) => {
                let total = k1.saturating_sub(k0) + u1.saturating_sub(u0);
                (total > 0).then(|| 1.0 - i1.saturating_sub(i0) as f64 / total as f64)
            }
            _ => None,
        };
        (cpu.map(|c| c.clamp(0.0, 1.0)), gpu)
    }
}

#[cfg(not(windows))]
mod imp {
    /// `(idle, total)` jiffies from the aggregate `cpu` line of /proc/stat.
    fn proc_stat() -> Option<(u64, u64)> {
        let text = std::fs::read_to_string("/proc/stat").ok()?;
        let line = text.lines().find(|l| l.starts_with("cpu "))?;
        let v: Vec<u64> = line
            .split_whitespace()
            .skip(1)
            .filter_map(|x| x.parse().ok())
            .collect();
        if v.len() < 4 {
            return None;
        }
        let idle = v[3] + v.get(4).copied().unwrap_or(0);
        Some((idle, v.iter().sum()))
    }

    pub fn load_sample(window: Duration) -> (Option<f64>, Option<f64>) {
        let a = proc_stat();
        std::thread::sleep(window);
        let b = proc_stat();
        let cpu = match (a, b) {
            (Some((i0, t0)), Some((i1, t1))) if t1 > t0 => {
                Some((1.0 - (i1 - i0) as f64 / (t1 - t0) as f64).clamp(0.0, 1.0))
            }
            _ => None,
        };
        (cpu, None)
    }

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
    fn busy_needs_a_known_high_figure() {
        assert!(!is_busy(None, None));
        assert!(!is_busy(Some(0.2), Some(0.1)));
        assert!(is_busy(Some(0.8), None));
        assert!(is_busy(None, Some(0.6)));
    }

    #[test]
    fn a_load_sample_is_in_range() {
        let (cpu, gpu) = load_sample(Duration::from_millis(200));
        assert!(cpu.is_none_or(|c| (0.0..=1.0).contains(&c)));
        assert!(gpu.is_none_or(|g| (0.0..=1.0).contains(&g)));
    }

    #[test]
    fn mains_state_from_supplies() {
        assert_eq!(ac_from_supplies(true, true), Some(true), "mains online → on AC");
        assert_eq!(ac_from_supplies(true, false), Some(false), "mains present but offline → on battery");
        assert_eq!(ac_from_supplies(false, false), None, "no mains supply → unknown, never guessed");
    }
}
