//! Overnight learning: when an app's memory engines do their expensive work.
//!
//! Memorabilia's overnight pass (chunking, embedding, recurrence, extraction)
//! and, when the app asks, adaptive-pathway's learn passes run inside a local
//! time window — by default 00:00 to 05:00 — instead of while the user is at
//! the machine. Per app (`/api/apps/me/overnight`), because the engine is
//! shared and does this work whether or not the app is open. An app with no
//! settings keeps the old behaviour: work on every sweep tick.
//!
//! Inside the window a pass starts once the user has been away 10 minutes,
//! stops when they come back, stops at the window's end, and runs to
//! completion at most once per window. A *catch-up* runs when the last full
//! pass is over 30 hours old, the machine has been idle 20 minutes and is on
//! mains power; *Process now* opens a one-hour window at once. While a pass
//! has work, the system is kept awake (display off); waking the machine is
//! the app's job (Kitty registers a scheduled task), and putting it back to
//! sleep is Windows' own unattended-wake timeout.

pub mod power;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, NaiveDateTime, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::storage::app_overnight;

/// A pass inside the window waits until the user has been away this long.
pub const WINDOW_IDLE: Duration = Duration::from_secs(10 * 60);
/// A catch-up waits until the user has been away this long.
pub const CATCH_UP_IDLE: Duration = Duration::from_secs(20 * 60);
/// A running pass stops when there was input more recently than this.
pub const USER_BACK: Duration = Duration::from_secs(2 * 60);
/// A catch-up is due when the last full pass is older than this.
pub const CATCH_UP_AFTER: chrono::Duration = chrono::Duration::hours(30);
/// How long a catch-up or *Process now* may run.
pub const BURST: Duration = Duration::from_secs(60 * 60);
/// Each sign of work keeps the system awake this much longer.
pub const AWAKE_LEASE: Duration = Duration::from_secs(3 * 60);
/// Pathway sessions learned per sweep tick inside the window (the daytime
/// default is the pathway sweep's own `IDLE_SWEEP_BATCH`).
pub const NIGHT_LEARN_BATCH: usize = 10;

/// An app's settings, as stored and as PUT.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OvernightConfig {
    /// Off: work runs on every sweep tick, as without settings.
    pub enabled: bool,
    /// Local time, `HH:MM`; the window may wrap midnight.
    pub start: String,
    pub end: String,
    /// Move adaptive-pathway's learning into the window as well.
    pub pathway: bool,
}

impl Default for OvernightConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            start: "00:00".into(),
            end: "05:00".into(),
            pathway: true,
        }
    }
}

fn parse_hm(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s.trim(), "%H:%M").ok()
}

impl OvernightConfig {
    pub fn validate(&self) -> Result<(), String> {
        let (Some(a), Some(b)) = (parse_hm(&self.start), parse_hm(&self.end)) else {
            return Err("start and end must be local times as HH:MM".into());
        };
        if a == b {
            return Err("the window must not be empty (start equals end)".into());
        }
        Ok(())
    }

    /// The window containing `t` (local), as `(start, end)`, or `None`.
    pub fn window_at(&self, t: NaiveDateTime) -> Option<(NaiveDateTime, NaiveDateTime)> {
        let (s, e) = (parse_hm(&self.start)?, parse_hm(&self.end)?);
        let day = t.date();
        let tt = t.time();
        if s < e {
            (s <= tt && tt < e).then(|| (day.and_time(s), day.and_time(e)))
        } else if tt >= s {
            Some((day.and_time(s), day.succ_opt()?.and_time(e)))
        } else if tt < e {
            Some((day.pred_opt()?.and_time(s), day.and_time(e)))
        } else {
            None
        }
    }
}

/// Why a pass may run now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    Window,
    CatchUp,
    RunNow,
}

/// The answer to "may memorabilia work now?".
#[derive(Debug, Clone, PartialEq)]
pub enum Permit {
    /// No window configured: work every tick, as before.
    Always,
    /// Work until `deadline`; `watch_user` stops it when the user returns.
    Now {
        trigger: Trigger,
        deadline: Instant,
        watch_user: bool,
    },
    NotNow,
}

/// Inputs to the permit decision, separated from the OS so it is testable.
#[derive(Debug, Clone)]
pub struct Situation {
    pub local_now: NaiveDateTime,
    pub utc_now: DateTime<Utc>,
    pub idle: Option<Duration>,
    pub on_ac: Option<bool>,
    pub run_now_until: Option<DateTime<Utc>>,
    pub last_finished: Option<DateTime<Utc>>,
    /// When the settings were first saved: a catch-up is never due before a
    /// window could have run.
    pub configured_at: Option<DateTime<Utc>>,
}

fn idle_at_least(idle: Option<Duration>, d: Duration) -> bool {
    // An OS that cannot say counts as away: the window still bounds the work.
    idle.is_none_or(|i| i >= d)
}

/// Pure permit decision.
pub fn decide(cfg: Option<&OvernightConfig>, s: &Situation) -> (Permit, Option<DateTime<Utc>>) {
    let Some(cfg) = cfg.filter(|c| c.enabled) else {
        return (Permit::Always, None);
    };
    let to_instant = |deadline: DateTime<Utc>| {
        let left = (deadline - s.utc_now).to_std().unwrap_or(Duration::ZERO);
        Instant::now() + left
    };
    if let Some(until) = s.run_now_until.filter(|u| *u > s.utc_now) {
        return (
            Permit::Now {
                trigger: Trigger::RunNow,
                deadline: to_instant(until),
                watch_user: false,
            },
            Some(until),
        );
    }
    if let Some((start, end)) = cfg.window_at(s.local_now) {
        let start_utc = Local
            .from_local_datetime(&start)
            .earliest()
            .map(|d| d.with_timezone(&Utc));
        let end_utc = Local
            .from_local_datetime(&end)
            .latest()
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or(s.utc_now);
        let done = match (s.last_finished, start_utc) {
            (Some(f), Some(st)) => f >= st,
            _ => false,
        };
        if !done && idle_at_least(s.idle, WINDOW_IDLE) {
            return (
                Permit::Now {
                    trigger: Trigger::Window,
                    deadline: to_instant(end_utc),
                    watch_user: true,
                },
                start_utc,
            );
        }
        return (Permit::NotNow, None);
    }
    let reference = s.last_finished.or(s.configured_at);
    let due = reference.is_some_and(|r| s.utc_now - r > CATCH_UP_AFTER);
    if due && idle_at_least(s.idle, CATCH_UP_IDLE) && s.on_ac != Some(false) {
        let until = s.utc_now + chrono::Duration::from_std(BURST).unwrap_or_default();
        return (
            Permit::Now {
                trigger: Trigger::CatchUp,
                deadline: to_instant(until),
                watch_user: true,
            },
            None,
        );
    }
    (Permit::NotNow, None)
}

fn parse_ts(s: Option<&str>) -> Option<DateTime<Utc>> {
    let s = s?;
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|n| Utc.from_utc_datetime(&n))
        })
}

/// What `GET /api/apps/me/overnight` reports.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub config: Option<OvernightConfig>,
    pub last_started: Option<String>,
    pub last_finished: Option<String>,
    pub last_report: Option<serde_json::Value>,
    pub running: bool,
    pub run_now_until: Option<String>,
    pub keeping_awake: bool,
    pub idle_secs: Option<u64>,
    pub on_ac_power: Option<bool>,
}

/// Daemon-wide: settings per app, *Process now* requests, keep-awake.
pub struct OvernightHub {
    pool: SqlitePool,
    run_now: Mutex<HashMap<String, DateTime<Utc>>>,
    running: Mutex<HashMap<String, bool>>,
    awake: power::KeepAwake,
}

impl OvernightHub {
    pub fn new(pool: SqlitePool) -> Arc<Self> {
        Arc::new(Self {
            pool,
            run_now: Mutex::new(HashMap::new()),
            running: Mutex::new(HashMap::new()),
            awake: power::KeepAwake::default(),
        })
    }

    async fn row(&self, app_id: &str) -> Option<app_overnight::Row> {
        match app_overnight::get(&self.pool, app_id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("overnight: could not read settings for {app_id}: {e}");
                None
            }
        }
    }

    /// The app's settings, if it has any.
    pub async fn config(&self, app_id: &str) -> Option<OvernightConfig> {
        let row = self.row(app_id).await?;
        serde_json::from_str(&row.config).ok()
    }

    pub async fn set_config(&self, app_id: &str, cfg: &OvernightConfig) -> Result<(), String> {
        cfg.validate()?;
        let json = serde_json::to_string(cfg).map_err(|e| e.to_string())?;
        app_overnight::set_config(&self.pool, app_id, &json)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn clear(&self, app_id: &str) -> Result<bool, String> {
        app_overnight::clear(&self.pool, app_id)
            .await
            .map_err(|e| e.to_string())
    }

    /// Whether the app's turn-end pathway learn pass is deferred to the
    /// window.
    pub async fn defers_pathway(&self, app_id: &str) -> bool {
        self.config(app_id)
            .await
            .is_some_and(|c| c.enabled && c.pathway)
    }

    /// Open a one-hour window now.
    pub fn run_now(&self, app_id: &str) {
        let until = Utc::now() + chrono::Duration::from_std(BURST).unwrap_or_default();
        if let Ok(mut m) = self.run_now.lock() {
            m.insert(app_id.to_string(), until);
        }
    }

    fn run_now_until(&self, app_id: &str) -> Option<DateTime<Utc>> {
        self.run_now.lock().ok()?.get(app_id).copied()
    }

    fn clear_run_now(&self, app_id: &str) {
        if let Ok(mut m) = self.run_now.lock() {
            m.remove(app_id);
        }
    }

    async fn situation(&self, app_id: &str, row: Option<&app_overnight::Row>) -> Situation {
        let configured_at = sqlx::query_scalar::<_, String>(
            "SELECT updated_at FROM app_overnight WHERE app_id = ?",
        )
        .bind(app_id)
        .fetch_optional(&self.pool)
        .await
        .ok()
        .flatten();
        Situation {
            local_now: Local::now().naive_local(),
            utc_now: Utc::now(),
            idle: power::idle_for(),
            on_ac: power::on_ac_power(),
            run_now_until: self.run_now_until(app_id),
            last_finished: parse_ts(row.and_then(|r| r.last_finished.as_deref())),
            configured_at: parse_ts(configured_at.as_deref()),
        }
    }

    /// May memorabilia's pass run for `app_id` now?
    pub async fn memory_permit(&self, app_id: &str) -> Permit {
        let row = self.row(app_id).await;
        let cfg = row.as_ref().and_then(|r| serde_json::from_str(&r.config).ok());
        let s = self.situation(app_id, row.as_ref()).await;
        decide(cfg.as_ref(), &s).0
    }

    /// How many pathway sessions may learn this tick for `app_id`: the
    /// default batch without settings, the night batch inside a permit,
    /// `None` outside it.
    pub async fn pathway_batch(&self, app_id: &str) -> Option<usize> {
        let row = self.row(app_id).await;
        let cfg: Option<OvernightConfig> =
            row.as_ref().and_then(|r| serde_json::from_str(&r.config).ok());
        let Some(cfg) = cfg.filter(|c| c.enabled && c.pathway) else {
            return Some(adaptive_pathway::background::IDLE_SWEEP_BATCH);
        };
        // Pathway shares memorabilia's triggers but not its once-per-window
        // rule: it learns whatever sessions have new messages for as long as
        // the window is open and the user away.
        let s = self.situation(app_id, row.as_ref()).await;
        let in_window = cfg.window_at(s.local_now).is_some() && idle_at_least(s.idle, WINDOW_IDLE);
        match decide(Some(&cfg), &s).0 {
            Permit::Now { .. } => Some(NIGHT_LEARN_BATCH),
            _ if in_window => Some(NIGHT_LEARN_BATCH),
            _ => None,
        }
    }

    /// Keep the system awake a little longer: there is work.
    pub fn working(&self) {
        self.awake.extend(AWAKE_LEASE);
    }

    /// Bookkeeping around one memorabilia pass.
    pub async fn pass_started(&self, app_id: &str) {
        if let Ok(mut m) = self.running.lock() {
            m.insert(app_id.to_string(), true);
        }
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let _ = app_overnight::mark_started(&self.pool, app_id, &now).await;
    }

    pub async fn pass_ended(&self, app_id: &str, trigger: Option<Trigger>, report: &serde_json::Value, finished: bool) {
        if let Ok(mut m) = self.running.lock() {
            m.remove(app_id);
        }
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let mut report = report.clone();
        if let (Some(obj), Some(t)) = (report.as_object_mut(), trigger) {
            obj.insert("trigger".into(), serde_json::json!(t));
            obj.insert("ended_at".into(), serde_json::json!(now));
        }
        let _ = app_overnight::record_report(
            &self.pool,
            app_id,
            &report.to_string(),
            finished.then_some(now.as_str()),
        )
        .await;
        if finished && trigger == Some(Trigger::RunNow) {
            self.clear_run_now(app_id);
        }
    }

    pub async fn status(&self, app_id: &str) -> Status {
        let row = self.row(app_id).await;
        Status {
            config: row.as_ref().and_then(|r| serde_json::from_str(&r.config).ok()),
            last_started: row.as_ref().and_then(|r| r.last_started.clone()),
            last_finished: row.as_ref().and_then(|r| r.last_finished.clone()),
            last_report: row
                .as_ref()
                .and_then(|r| r.last_report.as_deref())
                .and_then(|s| serde_json::from_str(s).ok()),
            running: self
                .running
                .lock()
                .map(|m| m.contains_key(app_id))
                .unwrap_or(false),
            run_now_until: self
                .run_now_until(app_id)
                .filter(|u| *u > Utc::now())
                .map(|u| u.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
            keeping_awake: self.awake.is_live(),
            idle_secs: power::idle_for().map(|d| d.as_secs()),
            on_ac_power: power::on_ac_power(),
        }
    }
}

/// The pass control memorabilia's overnight pass runs under.
pub struct PassGuard {
    pub hub: Arc<OvernightHub>,
    pub deadline: Instant,
    pub watch_user: bool,
}

impl memorabilia::learn::overnight::PassControl for PassGuard {
    fn keep_going(&self) -> bool {
        if Instant::now() >= self.deadline {
            return false;
        }
        if self.watch_user && power::idle_for().is_some_and(|i| i < USER_BACK) {
            return false;
        }
        self.hub.working();
        true
    }

    fn now(&self) -> String {
        Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }
}

/// Pathway's learn schedule for one app.
pub struct PathwaySchedule {
    pub hub: Arc<OvernightHub>,
    pub app_id: String,
}

#[async_trait::async_trait]
impl adaptive_pathway::traits::LearnSchedule for PathwaySchedule {
    async fn learn_batch(&self) -> Option<usize> {
        self.hub.pathway_batch(&self.app_id).await
    }

    fn working(&self) {
        self.hub.working();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(h: u32, m: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, 7)
            .unwrap()
            .and_hms_opt(h, m, 0)
            .unwrap()
    }

    fn situation(local: NaiveDateTime) -> Situation {
        let utc = Local
            .from_local_datetime(&local)
            .earliest()
            .unwrap()
            .with_timezone(&Utc);
        Situation {
            local_now: local,
            utc_now: utc,
            idle: Some(Duration::from_secs(3600)),
            on_ac: Some(true),
            run_now_until: None,
            last_finished: None,
            configured_at: Some(utc - chrono::Duration::hours(1)),
        }
    }

    #[test]
    fn windows_may_wrap_midnight() {
        let c = OvernightConfig::default();
        assert!(c.window_at(at(0, 0)).is_some());
        assert!(c.window_at(at(4, 59)).is_some());
        assert!(c.window_at(at(5, 0)).is_none());
        let wrap = OvernightConfig {
            start: "23:00".into(),
            end: "02:00".into(),
            ..Default::default()
        };
        let (s, e) = wrap.window_at(at(23, 30)).unwrap();
        assert_eq!((s.time(), e.date()), (at(23, 0).time(), at(0, 0).date().succ_opt().unwrap()));
        let (s, _) = wrap.window_at(at(1, 0)).unwrap();
        assert_eq!(s.date(), at(0, 0).date().pred_opt().unwrap());
        assert!(wrap.window_at(at(12, 0)).is_none());
        assert!(OvernightConfig { end: "00:00".into(), ..Default::default() }.validate().is_err());
        assert!(OvernightConfig { start: "25:00".into(), ..Default::default() }.validate().is_err());
    }

    #[test]
    fn no_settings_means_always() {
        assert_eq!(decide(None, &situation(at(12, 0))).0, Permit::Always);
        let off = OvernightConfig { enabled: false, ..Default::default() };
        assert_eq!(decide(Some(&off), &situation(at(12, 0))).0, Permit::Always);
    }

    #[test]
    fn the_window_waits_for_the_user_and_runs_once() {
        let c = OvernightConfig::default();
        let mut s = situation(at(1, 0));
        assert!(matches!(decide(Some(&c), &s).0, Permit::Now { trigger: Trigger::Window, .. }));
        s.idle = Some(Duration::from_secs(60));
        assert_eq!(decide(Some(&c), &s).0, Permit::NotNow);
        s.idle = Some(Duration::from_secs(3600));
        s.last_finished = Some(s.utc_now - chrono::Duration::minutes(30));
        assert_eq!(decide(Some(&c), &s).0, Permit::NotNow, "already finished this window");
        s.last_finished = Some(s.utc_now - chrono::Duration::hours(20));
        assert!(matches!(decide(Some(&c), &s).0, Permit::Now { .. }), "finished last night");
    }

    #[test]
    fn daytime_catch_up_needs_a_stale_run_idle_and_mains() {
        let c = OvernightConfig::default();
        let mut s = situation(at(14, 0));
        assert_eq!(decide(Some(&c), &s).0, Permit::NotNow, "configured an hour ago");
        s.last_finished = Some(s.utc_now - chrono::Duration::hours(40));
        assert!(matches!(decide(Some(&c), &s).0, Permit::Now { trigger: Trigger::CatchUp, .. }));
        s.on_ac = Some(false);
        assert_eq!(decide(Some(&c), &s).0, Permit::NotNow);
        s.on_ac = Some(true);
        s.idle = Some(Duration::from_secs(5 * 60));
        assert_eq!(decide(Some(&c), &s).0, Permit::NotNow);
    }

    #[test]
    fn process_now_ignores_the_window_and_the_user() {
        let c = OvernightConfig::default();
        let mut s = situation(at(14, 0));
        s.idle = Some(Duration::from_secs(1));
        s.run_now_until = Some(s.utc_now + chrono::Duration::minutes(30));
        assert!(matches!(
            decide(Some(&c), &s).0,
            Permit::Now { trigger: Trigger::RunNow, watch_user: false, .. }
        ));
    }

    #[tokio::test]
    async fn settings_round_trip_and_defer_pathway() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let hub = OvernightHub::new(pool);
        assert!(!hub.defers_pathway("kitty").await);
        assert_eq!(hub.pathway_batch("kitty").await, Some(adaptive_pathway::background::IDLE_SWEEP_BATCH));
        hub.set_config("kitty", &OvernightConfig::default()).await.unwrap();
        assert!(hub.defers_pathway("kitty").await);
        assert!(hub
            .set_config("kitty", &OvernightConfig { end: "00:00".into(), ..Default::default() })
            .await
            .is_err());
        hub.pass_started("kitty").await;
        hub.pass_ended("kitty", Some(Trigger::Window), &serde_json::json!({"chunks_promoted": 2}), true)
            .await;
        let st = hub.status("kitty").await;
        assert!(st.last_finished.is_some());
        assert_eq!(st.last_report.unwrap()["chunks_promoted"], 2);
        assert!(!st.running);
    }
}
