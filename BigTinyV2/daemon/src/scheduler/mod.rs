use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde_json::json;
use sqlx::SqlitePool;
use tokio_cron_scheduler::{Job, JobScheduler};

use crate::agent::Agent;
use crate::error::SchedulerError;
use crate::server::events::{SSEEvent, SSEEventType};
use crate::storage::execution;
use crate::storage::schedules::{self, ScheduleRow, ScheduleSpec, KIND_CRON, KIND_INTERVAL, KIND_ONCE};
use crate::storage::sessions;

/// Shortest interval an `interval` schedule may use. A tighter loop is a
/// runaway spend waiting to happen, not a schedule anyone means.
pub const MIN_INTERVAL_SECS: i64 = 60;

/// Longest a scheduled run's tool approval may wait. Matches the loop's
/// interactive ceiling (`agent::loop_::HITL_APPROVAL_TIMEOUT`).
pub const MAX_HITL_TIMEOUT_SECS: i64 = 3600;

/// Ports `plugins/bigtiny/bigtiny/scheduler/scheduler.py`, and extends it.
///
/// Three kinds of schedule (migration 025):
///
/// * `cron` -- `tokio-cron-scheduler` (backed by `croner`), which takes a
///   6-field expression (seconds first); `schedule_jobs.cron` stores standard
///   5-field crontab strings, so `to_seconds_cron` prepends a `0`.
/// * `interval` -- every `interval_secs`, measured from the previous run.
/// * `once` -- a single run at `run_at`, after which the schedule disables
///   itself.
///
/// The two timer kinds are driven by a task per schedule that sleeps until
/// the persisted `next_run_at`. Persisting it is what makes a run that fell due
/// while the daemon was down happen on the next start rather than silently
/// never: the task finds `next_run_at` in the past and runs at once. (Cron
/// keeps cron semantics -- a missed tick is not replayed.)
pub struct Scheduler {
    db: SqlitePool,
    /// A firing is an ordinary turn, so the scheduler drives the agent
    /// directly. Whether the work wants a specialist is the model's decision,
    /// made per firing, exactly as it would be in a chat.
    agent: Arc<Agent>,
    inner: JobScheduler,
    /// `schedule_jobs.id` -> `tokio-cron-scheduler`'s own job `Uuid`, so
    /// `update`/`remove` can unregister the *live* cron job. Without this,
    /// editing or deleting a schedule only ever touched the DB row and the old
    /// cron kept firing until the next restart.
    job_uuids: DashMap<String, uuid::Uuid>,
    /// `schedule_jobs.id` -> the timer task driving an `interval`/`once`
    /// schedule, for the same reason.
    timers: DashMap<String, tokio::task::AbortHandle>,
}

/// Jobs currently executing, keyed by `schedule_jobs.id`.
///
/// `tokio-cron-scheduler` spawns a fresh task for every due tick and derives
/// the next tick from the cron expression, never from when the previous run
/// finished. A `*/5` schedule whose run takes ten minutes therefore piled up
/// overlapping executions: concurrent provider spend, interleaved
/// `execution_history` rows, and two agents writing the same transcript.
///
/// Process-wide rather than a `Scheduler` field because `run_now` (the manual
/// trigger route) starts runs without the scheduler mutex, and a manual run
/// must contend with a timed run for the same slot.
static JOBS_IN_FLIGHT: once_cell::sync::Lazy<DashMap<String, ()>> =
    once_cell::sync::Lazy::new(DashMap::new);

/// Releases a job's in-flight slot on every exit path — early returns and
/// panics included.
struct InFlightGuard(String);

impl InFlightGuard {
    /// `None` when the job is already running, which is the signal to skip
    /// this tick entirely.
    fn claim(job_id: &str) -> Option<Self> {
        match JOBS_IN_FLIGHT.entry(job_id.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(_) => None,
            dashmap::mapref::entry::Entry::Vacant(slot) => {
                slot.insert(());
                Some(Self(job_id.to_string()))
            }
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        JOBS_IN_FLIGHT.remove(&self.0);
    }
}

fn to_seconds_cron(cron: &str) -> String {
    if cron.split_whitespace().count() == 5 {
        format!("0 {cron}")
    } else {
        cron.to_string()
    }
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}

/// Check a spec for the fields its kind needs, and fill in what the daemon
/// derives: `next_run_at` for a new timer schedule (or one whose timing
/// changed), and `''` for the cron column of the timer kinds.
///
/// `previous` is the schedule being edited, if any: when the timing fields
/// are unchanged its `next_run_at` is kept, so renaming a schedule or editing
/// its prompt does not push its next run back.
pub fn normalize_spec(spec: &mut ScheduleSpec, previous: Option<&ScheduleRow>) -> Result<(), SchedulerError> {
    let bad = |m: &str| Err(SchedulerError::Cron(m.to_string()));
    if spec.name.trim().is_empty() || spec.prompt.trim().is_empty() {
        return bad("name and prompt are required");
    }
    if !(10..=MAX_HITL_TIMEOUT_SECS).contains(&spec.hitl_timeout_secs) {
        return bad("hitl_timeout_secs must be between 10 and 3600");
    }
    let timing_changed = previous.is_none_or(|p| {
        p.kind != spec.kind
            || p.cron != spec.cron
            || p.interval_secs != spec.interval_secs
            || p.run_at != spec.run_at
            || (p.enabled == 0 && spec.enabled)
    });
    match spec.kind.as_str() {
        KIND_CRON => {
            if spec.cron.trim().is_empty() {
                return bad("a cron schedule needs `cron`");
            }
            // Validate without registering: an invalid expression must fail
            // the request before anything is persisted.
            Job::new_async(to_seconds_cron(&spec.cron).as_str(), |_, _| Box::pin(async {}))
                .map_err(|e| SchedulerError::Cron(e.to_string()))?;
            spec.interval_secs = None;
            spec.run_at = None;
            spec.next_run_at = None;
        }
        KIND_INTERVAL => {
            let Some(secs) = spec.interval_secs else {
                return bad("an interval schedule needs `interval_secs`");
            };
            if secs < MIN_INTERVAL_SECS {
                return bad("interval_secs must be at least 60");
            }
            spec.cron = String::new();
            spec.run_at = None;
            if timing_changed || spec.next_run_at.is_none() {
                // A chosen first run (in the future), else one interval out.
                let first = spec
                    .first_run_at
                    .take()
                    .as_deref()
                    .and_then(parse_time)
                    .filter(|t| *t > Utc::now());
                spec.next_run_at = Some(
                    first
                        .unwrap_or_else(|| Utc::now() + chrono::Duration::seconds(secs))
                        .to_rfc3339(),
                );
            }
        }
        KIND_ONCE => {
            let Some(at) = spec.run_at.as_deref().and_then(parse_time) else {
                return bad("a once schedule needs `run_at` as an RFC 3339 time");
            };
            spec.cron = String::new();
            spec.interval_secs = None;
            spec.run_at = Some(at.to_rfc3339());
            if timing_changed || spec.next_run_at.is_none() {
                spec.next_run_at = Some(at.to_rfc3339());
            }
        }
        other => return bad(&format!("unknown schedule kind {other:?}")),
    }
    Ok(())
}

impl Scheduler {
    pub async fn new(db: SqlitePool, agent: Arc<Agent>) -> Result<Self, SchedulerError> {
        let inner = JobScheduler::new()
            .await
            .map_err(|e| SchedulerError::Cron(e.to_string()))?;
        Ok(Self {
            db,
            agent,
            inner,
            job_uuids: DashMap::new(),
            timers: DashMap::new(),
        })
    }

    /// Load every `enabled` schedule and register it, then start the
    /// scheduler. Timer schedules whose `next_run_at` passed while the daemon
    /// was down run straight away.
    pub async fn start(&mut self) -> Result<(), SchedulerError> {
        let jobs = schedules::list_schedules(&self.db)
            .await
            .map_err(SchedulerError::from)?;
        let enabled: Vec<ScheduleRow> = jobs.into_iter().filter(|j| j.enabled != 0).collect();
        let count = enabled.len();

        for job in &enabled {
            if let Err(e) = self.register(job).await {
                tracing::warn!("Failed to schedule job {}: {}", job.id, e);
            }
        }

        self.inner
            .start()
            .await
            .map_err(|e| SchedulerError::Cron(e.to_string()))?;
        tracing::info!("Scheduler started with {count} jobs");
        Ok(())
    }

    /// Register a schedule's live trigger according to its kind.
    async fn register(&mut self, row: &ScheduleRow) -> Result<(), SchedulerError> {
        match row.kind.as_str() {
            KIND_INTERVAL | KIND_ONCE => {
                self.spawn_timer(&row.id);
                Ok(())
            }
            _ => self.register_cron_job(&row.id, &row.cron).await,
        }
    }

    async fn register_cron_job(&mut self, job_id: &str, cron: &str) -> Result<(), SchedulerError> {
        let cron_expr = to_seconds_cron(cron);
        let db = self.db.clone();
        let agent = self.agent.clone();
        let job_id_owned = job_id.to_string();

        let job = Job::new_async(cron_expr.as_str(), move |_uuid, _lock| {
            let db = db.clone();
            let agent = agent.clone();
            let job_id = job_id_owned.clone();
            Box::pin(async move {
                execute_job(&db, &agent, &job_id).await;
            })
        })
        .map_err(|e| SchedulerError::Cron(e.to_string()))?;

        let uuid = self
            .inner
            .add(job)
            .await
            .map_err(|e| SchedulerError::Cron(e.to_string()))?;
        self.job_uuids.insert(job_id.to_string(), uuid);
        Ok(())
    }

    /// Drive an `interval`/`once` schedule: sleep until its persisted
    /// `next_run_at`, run, then record the next due time (or, for `once`,
    /// disable the schedule). Re-reads the row each lap, so a schedule deleted
    /// or disabled out from under the task simply ends it.
    fn spawn_timer(&self, job_id: &str) {
        let db = self.db.clone();
        let agent = self.agent.clone();
        let id = job_id.to_string();
        let task = tokio::spawn(async move {
            loop {
                let Ok(Some(row)) = schedules::get_schedule(&db, &id).await else {
                    return;
                };
                if row.enabled == 0 {
                    return;
                }
                let due = row
                    .next_run_at
                    .as_deref()
                    .and_then(parse_time)
                    .unwrap_or_else(Utc::now);
                if let Ok(wait) = (due - Utc::now()).to_std() {
                    tokio::time::sleep(wait).await;
                }
                execute_job(&db, &agent, &id).await;
                let (next, still_enabled) = match row.kind.as_str() {
                    KIND_INTERVAL => {
                        let secs = row.interval_secs.unwrap_or(MIN_INTERVAL_SECS).max(MIN_INTERVAL_SECS);
                        // From now, not from `due`: a daemon that was down for
                        // a day catches up once, not once per missed interval.
                        (Some((Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339()), true)
                    }
                    _ => (None, false),
                };
                if let Err(e) = schedules::set_next_run(&db, &id, next.as_deref(), still_enabled).await {
                    tracing::error!("schedule {id}: could not record its next run: {e}");
                    return;
                }
                if !still_enabled {
                    return;
                }
            }
        });
        if let Some(old) = self.timers.insert(job_id.to_string(), task.abort_handle()) {
            old.abort();
        }
    }

    /// Unregister `job_id`'s live trigger (cron job or timer task), if any.
    async fn unregister(&mut self, job_id: &str) {
        if let Some((_, uuid)) = self.job_uuids.remove(job_id) {
            if let Err(e) = self.inner.remove(&uuid).await {
                tracing::warn!("Failed to unregister cron job {job_id}: {e}");
            }
        }
        if let Some((_, timer)) = self.timers.remove(job_id) {
            timer.abort();
        }
    }

    /// When a schedule next fires: `next_run_at` for the timer kinds, the
    /// cron engine's own next tick for cron. `None` when disabled.
    pub async fn next_run(&mut self, row: &ScheduleRow) -> Option<String> {
        if row.enabled == 0 {
            return None;
        }
        if row.kind != KIND_CRON {
            return row.next_run_at.clone();
        }
        let uuid = *self.job_uuids.get(&row.id)?;
        self.inner
            .next_tick_for_job(uuid)
            .await
            .ok()
            .flatten()
            .map(|t| t.to_rfc3339())
    }

    /// Apply a cron/enabled edit (the original, narrower update). Kept for
    /// existing callers; it goes through [`Self::update_schedule`].
    pub async fn update_job(
        &mut self,
        job_id: &str,
        cron: Option<&str>,
        enabled: Option<bool>,
    ) -> Result<(), SchedulerError> {
        let current = schedules::get_schedule(&self.db, job_id)
            .await
            .map_err(SchedulerError::from)?
            .ok_or_else(|| SchedulerError::NotFound(job_id.to_string()))?;
        let mut spec = ScheduleSpec::from_row(&current);
        if let Some(c) = cron {
            spec.cron = c.to_string();
        }
        if let Some(e) = enabled {
            spec.enabled = e;
        }
        self.update_schedule(job_id, spec).await
    }

    /// Replace a schedule's definition *and* its live trigger.
    ///
    /// The spec is validated before anything changes, so an invalid edit
    /// leaves both the row and the running trigger exactly as they were.
    /// Then: persist, drop the old trigger, register the new one if enabled.
    /// If registering fails after the row was written (which validation makes
    /// very unlikely), the old row is restored and re-registered, so the DB
    /// and the live scheduler never disagree.
    pub async fn update_schedule(
        &mut self,
        job_id: &str,
        mut spec: ScheduleSpec,
    ) -> Result<(), SchedulerError> {
        let current = schedules::get_schedule(&self.db, job_id)
            .await
            .map_err(SchedulerError::from)?
            .ok_or_else(|| SchedulerError::NotFound(job_id.to_string()))?;
        normalize_spec(&mut spec, Some(&current))?;

        schedules::update_schedule_spec(&self.db, job_id, &spec)
            .await
            .map_err(SchedulerError::from)?;
        self.unregister(job_id).await;
        if !spec.enabled {
            return Ok(());
        }
        let updated = schedules::get_schedule(&self.db, job_id)
            .await
            .map_err(SchedulerError::from)?
            .ok_or_else(|| SchedulerError::NotFound(job_id.to_string()))?;
        if let Err(e) = self.register(&updated).await {
            let _ = schedules::update_schedule_spec(&self.db, job_id, &ScheduleSpec::from_row(&current)).await;
            if current.enabled != 0 {
                let _ = self.register(&current).await;
            }
            return Err(e);
        }
        Ok(())
    }

    /// Delete a schedule row *and* unregister its live trigger.
    pub async fn remove_job(&mut self, job_id: &str) -> Result<u64, SchedulerError> {
        self.unregister(job_id).await;
        schedules::delete_schedule(&self.db, job_id)
            .await
            .map_err(SchedulerError::from)
    }

    /// Create a plain cron schedule (the original, narrower create). Kept for
    /// existing callers; it goes through [`Self::add_schedule`].
    pub async fn add_job(
        &mut self,
        name: &str,
        cron: &str,
        prompt: &str,
        enabled: bool,
        app_id: &str,
    ) -> Result<String, SchedulerError> {
        self.add_schedule(
            app_id,
            ScheduleSpec {
                name: name.to_string(),
                prompt: prompt.to_string(),
                kind: KIND_CRON.to_string(),
                cron: cron.to_string(),
                hitl_timeout_secs: 600,
                enabled,
                ..Default::default()
            },
        )
        .await
    }

    /// Create a schedule of any kind and (if enabled) register its trigger
    /// immediately, without requiring a scheduler restart.
    pub async fn add_schedule(
        &mut self,
        app_id: &str,
        mut spec: ScheduleSpec,
    ) -> Result<String, SchedulerError> {
        normalize_spec(&mut spec, None)?;
        // Full UUIDs: at the old 8 hex chars a collision was realistic, and a
        // colliding id would overwrite another job's live registration.
        let id = uuid::Uuid::new_v4().to_string();
        schedules::create_schedule_spec(&self.db, &id, app_id, &spec)
            .await
            .map_err(SchedulerError::from)?;
        if spec.enabled {
            let row = schedules::get_schedule(&self.db, &id)
                .await
                .map_err(SchedulerError::from)?
                .ok_or_else(|| SchedulerError::NotFound(id.clone()))?;
            if let Err(e) = self.register(&row).await {
                // A row with no live trigger would silently never fire.
                let _ = schedules::delete_schedule(&self.db, &id).await;
                return Err(e);
            }
        }
        Ok(id)
    }

    /// Execute one scheduled job to completion. Returns `false` (not an
    /// error) when the job is genuinely missing, so a caller can distinguish
    /// 404 from a real storage failure (500).
    pub async fn run_job(&self, job_id: &str) -> Result<bool, SchedulerError> {
        let job = schedules::get_schedule(&self.db, job_id)
            .await
            .map_err(SchedulerError::from)?;
        let Some(job) = job else {
            return Ok(false);
        };
        execute_job(&self.db, &self.agent, &job.id).await;
        Ok(true)
    }

    /// Whether a scheduled or manual job is mid-run.
    ///
    /// Read by the idle-exit timer. A scheduled run has no client at all --
    /// that is the point of it -- so it is exactly the work the activity clock
    /// cannot see, and exactly the work it would be worst to kill halfway.
    pub fn has_running_jobs(&self) -> bool {
        !JOBS_IN_FLIGHT.is_empty()
    }

    pub async fn stop(&mut self) {
        for entry in self.timers.iter() {
            entry.value().abort();
        }
        self.timers.clear();
        if let Err(e) = self.inner.shutdown().await {
            tracing::warn!("Scheduler shutdown error: {e}");
        }
        tracing::info!("Scheduler stopped");
    }
}

/// Why a manual run could not start.
#[derive(Debug)]
pub enum StartRunError {
    NotFound,
    Disabled,
    AlreadyRunning,
    Storage(String),
}

/// Start a run of `job_id` now and return its session id without waiting for
/// it to finish -- the route behind "Run now". The run itself continues in the
/// background exactly as a timed one would.
pub async fn start_job_now(
    db: &SqlitePool,
    agent: &Arc<Agent>,
    job_id: &str,
) -> Result<String, StartRunError> {
    let guard = InFlightGuard::claim(job_id).ok_or(StartRunError::AlreadyRunning)?;
    let job = match schedules::get_schedule(db, job_id).await {
        Ok(Some(job)) => job,
        Ok(None) => return Err(StartRunError::NotFound),
        Err(e) => return Err(StartRunError::Storage(e.to_string())),
    };
    if job.enabled == 0 {
        return Err(StartRunError::Disabled);
    }
    let run = prepare_run(db, agent, &job)
        .await
        .map_err(StartRunError::Storage)?;
    let session_id = run.session_id.clone();
    let db = db.clone();
    let agent = agent.clone();
    tokio::spawn(async move {
        let _guard = guard;
        finish_run(&db, &agent, &job, run).await;
    });
    Ok(session_id)
}

/// Execute one scheduled job: run session + `execution_history` bookkeeping,
/// then the turn. Used by the cron engine, the timer tasks and `run_job`.
///
/// The session is kept whatever happens: it is the run's output, and a failed
/// run's transcript is the only record of why it failed. It ages out through
/// the same retention sweep as any other session.
pub(crate) async fn execute_job(db: &SqlitePool, agent: &Arc<Agent>, job_id: &str) {
    // Held for the whole execution; dropped on every return path below.
    let Some(_in_flight) = InFlightGuard::claim(job_id) else {
        tracing::warn!("scheduled job {job_id}: previous run still in flight; skipping this tick");
        return;
    };

    let Ok(Some(job)) = schedules::get_schedule(db, job_id).await else {
        // A DB error here used to be invisible — log it so a schedule that
        // silently stopped firing isn't indistinguishable from a dead daemon.
        tracing::error!("scheduled job {job_id}: failed to load schedule row from db");
        return;
    };

    // Never run a job whose row says `enabled = 0`. The live trigger is
    // (un)registered to match the row, but the two can diverge -- and a
    // disabled job must not run regardless of what fired.
    if job.enabled == 0 {
        tracing::debug!("scheduled job {job_id}: skipping, schedule is disabled");
        return;
    }

    match prepare_run(db, agent, &job).await {
        Ok(run) => finish_run(db, agent, &job, run).await,
        Err(e) => tracing::error!("scheduled job {job_id}: {e}"),
    }
}

/// A run whose session and audit row exist but whose turn has not started.
struct PreparedRun {
    exec_id: String,
    session_id: String,
}

fn schedule_event(job: &ScheduleRow, session_id: &str, outcome: &str, error: Option<String>) -> SSEEvent {
    SSEEvent {
        event_type: SSEEventType::ScheduleRun,
        schedule_id: Some(job.id.clone()),
        session_id: Some(session_id.to_string()),
        content: Some(outcome.to_string()),
        tool_name: Some(job.name.clone()),
        error_message: error,
        ..Default::default()
    }
}

/// Create the run's session -- owned by the schedule's app and configured the
/// way the schedule says (provider pin, persona, approval wait) -- and its
/// `execution_history` row, and announce that the run started.
async fn prepare_run(db: &SqlitePool, agent: &Arc<Agent>, job: &ScheduleRow) -> Result<PreparedRun, String> {
    let exec_id = uuid::Uuid::new_v4().simple().to_string();
    let session_id = format!("job_{exec_id}");
    // Owned by the app that owns the schedule: an ownerless session is
    // unreachable by every scoped accessor in `storage::sessions`, so the run
    // would produce a transcript nobody could read.
    sessions::create_session_for_app(db, &session_id, &format!("scheduled: {}", job.name), &job.app_id)
        .await
        .map_err(|e| format!("failed to create the run session: {e}"))?;
    let _ = sessions::update_session_status(db, &session_id, "idle").await;

    let mut meta = json!({
        "schedule_id": job.id,
        // Nobody is watching a scheduled run by default, so it waits minutes
        // for an approval, not the hour an interactive session gets.
        "hitl_timeout_secs": job.hitl_timeout_secs,
    });
    if let Some(provider) = job.provider_id.as_deref().filter(|p| !p.is_empty()) {
        meta["provider"] = json!(provider);
        meta["model"] = json!(job.model.clone().unwrap_or_default());
    }
    if let Some(prompt) = job.system_prompt.as_deref().filter(|p| !p.trim().is_empty()) {
        meta["persona_override"] = json!(prompt);
    }
    // Where the run works: its folder is its own, as a session created with
    // a folder has (`routes::chat::create_session`).
    if let Some(cwd) = job.cwd.as_deref().filter(|c| !c.trim().is_empty()) {
        meta["cwd"] = json!(cwd);
        meta["chat_dir"] = json!(cwd);
    }
    if let Err(e) = sessions::update_session_config(db, &session_id, &meta.to_string()).await {
        let _ = sessions::delete_session(db, &session_id).await;
        return Err(format!("failed to configure the run session: {e}"));
    }

    if let Err(e) = execution::insert_execution(db, &exec_id, &session_id, "schedule", Some(&job.id)).await {
        let _ = sessions::delete_session(db, &session_id).await;
        return Err(format!("failed to insert the execution row: {e}"));
    }
    let _ = schedules::record_last_run(db, &job.id, "running", &session_id).await;
    agent
        .app_events()
        .publish(&job.app_id, schedule_event(job, &session_id, "started", None));
    Ok(PreparedRun { exec_id, session_id })
}

/// Run the turn and record how it went: the audit row, the schedule's
/// last-run summary, and a `schedule_run` event for the owning app.
async fn finish_run(db: &SqlitePool, agent: &Arc<Agent>, job: &ScheduleRow, run: PreparedRun) {
    let PreparedRun { exec_id, session_id } = run;
    // Background priority: a scheduled run must never put a user's own message
    // behind it.
    let outcome = agent
        .run_turn_and_wait_outcome(&session_id, &job.prompt, crate::provider::queue::Priority::Background)
        .await;
    let (status, event_outcome, error) = match &outcome {
        Ok(o) if o.approvals_timed_out > 0 => (
            "completed_with_denied_tools",
            "denied_by_timeout",
            Some(format!(
                "{} tool call(s) were skipped because nobody approved them in time",
                o.approvals_timed_out
            )),
        ),
        Ok(_) => ("completed", "finished", None),
        Err(msg) => ("failed", "failed", Some(msg.clone())),
    };
    if status == "failed" {
        tracing::error!("Scheduled job {} failed: {}", job.id, error.as_deref().unwrap_or(""));
    }
    // `execution_history.status` keeps its original vocabulary; the finer
    // "completed with denied tools" lives on the schedule row and the event.
    let exec_status = if status == "failed" { "failed" } else { "completed" };
    if let Err(e) =
        execution::update_execution_status(db, &exec_id, exec_status, None, error.as_deref()).await
    {
        // A failed update leaves the row `running` forever; say so.
        tracing::error!("scheduled job {}: failed to mark execution {exec_id} {exec_status}: {e}", job.id);
    }
    let _ = schedules::record_last_run(db, &job.id, status, &session_id).await;
    agent
        .app_events()
        .publish(&job.app_id, schedule_event(job, &session_id, event_outcome, error));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: &str) -> ScheduleSpec {
        ScheduleSpec {
            name: "n".into(),
            prompt: "p".into(),
            kind: kind.into(),
            hitl_timeout_secs: 600,
            enabled: true,
            ..Default::default()
        }
    }

    /// An interval schedule can be told when to start; a past or missing
    /// time falls back to one interval from now.
    #[test]
    fn an_interval_schedule_starts_at_its_chosen_first_run() {
        let first = Utc::now() + chrono::Duration::minutes(10);
        let mut s = ScheduleSpec {
            interval_secs: Some(3600),
            first_run_at: Some(first.to_rfc3339()),
            ..spec(KIND_INTERVAL)
        };
        normalize_spec(&mut s, None).unwrap();
        let next = parse_time(s.next_run_at.as_deref().unwrap()).unwrap();
        assert!((next - first).num_seconds().abs() <= 1);

        let mut past = ScheduleSpec {
            interval_secs: Some(3600),
            first_run_at: Some((Utc::now() - chrono::Duration::hours(1)).to_rfc3339()),
            ..spec(KIND_INTERVAL)
        };
        normalize_spec(&mut past, None).unwrap();
        let next = parse_time(past.next_run_at.as_deref().unwrap()).unwrap();
        assert!(next > Utc::now() + chrono::Duration::minutes(59));
    }

    #[test]
    fn each_kind_requires_its_own_timing_field() {
        assert!(normalize_spec(&mut spec(KIND_CRON), None).is_err());
        assert!(normalize_spec(&mut spec(KIND_INTERVAL), None).is_err());
        assert!(normalize_spec(&mut spec(KIND_ONCE), None).is_err());
        assert!(normalize_spec(&mut spec("hourly"), None).is_err());

        let mut s = ScheduleSpec { cron: "0 9 * * *".into(), ..spec(KIND_CRON) };
        assert!(normalize_spec(&mut s, None).is_ok());
        let mut s = ScheduleSpec { cron: "not a cron".into(), ..spec(KIND_CRON) };
        assert!(normalize_spec(&mut s, None).is_err());
    }

    #[test]
    fn an_interval_gets_a_next_run_and_a_floor() {
        let mut s = ScheduleSpec { interval_secs: Some(30), ..spec(KIND_INTERVAL) };
        assert!(normalize_spec(&mut s, None).is_err(), "sub-minute intervals are refused");
        let mut s = ScheduleSpec { interval_secs: Some(3600), ..spec(KIND_INTERVAL) };
        normalize_spec(&mut s, None).unwrap();
        let next = parse_time(s.next_run_at.as_deref().unwrap()).unwrap();
        let ahead = (next - Utc::now()).num_seconds();
        assert!((3590..=3600).contains(&ahead), "{ahead}");
        assert_eq!(s.cron, "");
    }

    #[test]
    fn a_once_schedule_is_due_at_its_run_at() {
        let at = "2031-01-02T03:04:05Z";
        let mut s = ScheduleSpec { run_at: Some(at.into()), ..spec(KIND_ONCE) };
        normalize_spec(&mut s, None).unwrap();
        assert_eq!(
            parse_time(s.next_run_at.as_deref().unwrap()),
            parse_time(at)
        );
        let mut bad = ScheduleSpec { run_at: Some("tomorrow".into()), ..spec(KIND_ONCE) };
        assert!(normalize_spec(&mut bad, None).is_err());
    }

    /// Editing a schedule's name or prompt must not push its next run back --
    /// only a change to its timing (or re-enabling it) recomputes it.
    #[test]
    fn an_edit_that_leaves_the_timing_alone_keeps_the_next_run() {
        let kept = "2031-05-05T00:00:00+00:00".to_string();
        let previous = ScheduleRow {
            id: "s".into(),
            name: "old".into(),
            cron: String::new(),
            prompt: "p".into(),
            enabled: 1,
            created_at: None,
            updated_at: None,
            app_id: "a".into(),
            kind: KIND_INTERVAL.into(),
            interval_secs: Some(3600),
            run_at: None,
            next_run_at: Some(kept.clone()),
            provider_id: None,
            model: None,
            system_prompt: None,
            hitl_timeout_secs: 600,
            cwd: None,
            last_run_at: None,
            last_status: None,
            last_session_id: None,
        };
        let mut renamed = ScheduleSpec { name: "new".into(), ..ScheduleSpec::from_row(&previous) };
        normalize_spec(&mut renamed, Some(&previous)).unwrap();
        assert_eq!(renamed.next_run_at.as_deref(), Some(kept.as_str()));

        let mut retimed = ScheduleSpec { interval_secs: Some(7200), ..ScheduleSpec::from_row(&previous) };
        normalize_spec(&mut retimed, Some(&previous)).unwrap();
        assert_ne!(retimed.next_run_at.as_deref(), Some(kept.as_str()));
    }

    #[test]
    fn the_approval_wait_is_bounded() {
        let mut s = ScheduleSpec { cron: "0 9 * * *".into(), hitl_timeout_secs: 5, ..spec(KIND_CRON) };
        assert!(normalize_spec(&mut s, None).is_err());
        let mut s = ScheduleSpec { cron: "0 9 * * *".into(), hitl_timeout_secs: 7200, ..spec(KIND_CRON) };
        assert!(normalize_spec(&mut s, None).is_err());
    }
}
