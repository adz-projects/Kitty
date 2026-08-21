use std::sync::Arc;

use dashmap::DashMap;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tokio_cron_scheduler::{Job, JobScheduler};

use crate::error::SchedulerError;
use crate::recipes::engine::RecipeEngine;
use crate::storage::execution;
use crate::storage::schedules::{self, ScheduleRow};
use crate::storage::sessions;

/// Ports `plugins/bigtiny/bigtiny/scheduler/scheduler.py`. Uses
/// `tokio-cron-scheduler` (backed by the `croner` crate) rather than
/// hand-rolling a `tokio::time::interval` poll loop — a bare poll loop would
/// still need its own cron-expression evaluator to decide "is this job due
/// right now," so it wouldn't actually be simpler, just a worse
/// reimplementation of what this crate already does.
///
/// `tokio-cron-scheduler`'s `Job::new_async` requires a 6-field cron
/// expression (seconds first); `schedule_jobs.cron` stores standard 5-field
/// crontab strings (matching Python's `CronTrigger.from_crontab`), so
/// `to_seconds_cron` prepends a `0` seconds field before handing it off.
pub struct Scheduler {
    db: SqlitePool,
    engine: Arc<RecipeEngine>,
    inner: JobScheduler,
    /// Maps our `schedule_jobs.id` to `tokio-cron-scheduler`'s own internal
    /// job `Uuid` (returned by `inner.add`, otherwise discarded) — needed so
    /// `update_job`/`remove_job` can find and unregister the *live* cron
    /// job. Without this, editing or deleting a schedule only ever touched
    /// the DB row: the old cron kept firing (or kept firing after being
    /// disabled) until the next full daemon restart.
    job_uuids: DashMap<String, uuid::Uuid>,
}

/// Jobs currently executing, keyed by `schedule_jobs.id`.
///
/// `tokio-cron-scheduler` spawns a fresh task for every due tick and derives
/// the next tick from the cron expression, never from when the previous run
/// finished. A `*/5` schedule whose recipe takes ten minutes therefore piled
/// up overlapping executions: concurrent provider spend, interleaved
/// `execution_history` rows, and two agents writing the same recipe's state.
///
/// Process-wide rather than a `Scheduler` field because `run_now` (the manual
/// trigger route) calls `execute_job` directly, and a manual run must contend
/// with the cron run for the same slot.
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

impl Scheduler {
    pub async fn new(db: SqlitePool, engine: Arc<RecipeEngine>) -> Result<Self, SchedulerError> {
        let inner = JobScheduler::new()
            .await
            .map_err(|e| SchedulerError::Cron(e.to_string()))?;
        Ok(Self {
            db,
            engine,
            inner,
            job_uuids: DashMap::new(),
        })
    }

    /// Load every `enabled` job and register it, then start the scheduler.
    pub async fn start(&mut self) -> Result<(), SchedulerError> {
        let jobs = schedules::list_schedules(&self.db)
            .await
            .map_err(SchedulerError::from)?;
        let enabled: Vec<ScheduleRow> = jobs.into_iter().filter(|j| j.enabled != 0).collect();
        let count = enabled.len();

        for job in &enabled {
            if let Err(e) = self.register_cron_job(&job.id, &job.cron).await {
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

    async fn register_cron_job(&mut self, job_id: &str, cron: &str) -> Result<(), SchedulerError> {
        let cron_expr = to_seconds_cron(cron);
        let db = self.db.clone();
        let engine = self.engine.clone();
        let job_id_owned = job_id.to_string();

        let job = Job::new_async(cron_expr.as_str(), move |_uuid, _lock| {
            let db = db.clone();
            let engine = engine.clone();
            let job_id = job_id_owned.clone();
            Box::pin(async move {
                execute_job(&db, &engine, &job_id).await;
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

    /// Unregister `job_id`'s live cron job, if one is currently registered.
    /// A no-op if it was never registered (e.g. it was already disabled).
    async fn unregister_cron_job(&mut self, job_id: &str) {
        if let Some((_, uuid)) = self.job_uuids.remove(job_id) {
            if let Err(e) = self.inner.remove(&uuid).await {
                tracing::warn!("Failed to unregister cron job {job_id}: {e}");
            }
        }
    }

    /// Apply a cron/enabled edit to a schedule row *and* the live scheduler
    /// — always unregisters the old cron job first (if any), then
    /// re-registers with the new cron only if the job ends up enabled.
    /// Matches Python's real mechanism (APScheduler `add_job`/remove on
    /// edit), not just a DB write.
    ///
    /// Ordering matters: the live cron is validated + registered *before* the
    /// DB is persisted, so an invalid cron (which `register_cron_job` rejects
    /// via `Job::new_async`/`inner.add`) leaves the row untouched and the old
    /// job still firing — previously the DB was updated first, then the
    /// register failed, leaving the row pointing at a cron that would never
    /// fire until the next restart. `enabled=false` needs no live
    /// registration; the old job is simply unregistered.
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

        let new_enabled = enabled.unwrap_or(current.enabled != 0);
        let new_cron = cron
            .map(|s| s.to_string())
            .unwrap_or_else(|| current.cron.clone());

        if new_enabled {
            // Take the existing registration mapping out first (without
            // touching the still-running scheduler job) so the new one can
            // take over its key...
            let old_uuid = self.job_uuids.remove(job_id).map(|(_, uuid)| uuid);
            match self.register_cron_job(job_id, &new_cron).await {
                Ok(()) => {
                    // New job proven registerable — now it is safe to retire
                    // the old one. Sequential `inner.add` then `inner.remove`
                    // means there's never a gap where the job isn't firing.
                    if let Some(old_uuid) = old_uuid {
                        if let Err(e) = self.inner.remove(&old_uuid).await {
                            tracing::warn!("Failed to unregister old cron job {job_id}: {e}");
                        }
                    }
                }
                Err(e) => {
                    // Register failed — restore the old mapping (the old live
                    // job was never removed) and return, leaving DB + live
                    // scheduler exactly as they were.
                    if let Some(old_uuid) = old_uuid {
                        self.job_uuids.insert(job_id.to_string(), old_uuid);
                    }
                    return Err(e);
                }
            }
        } else {
            self.unregister_cron_job(job_id).await;
        }

        // Persist last. The live cron was already re-registered above (to
        // validate it); if the DB write now fails, roll that live change back
        // so DB and the running scheduler don't diverge — the row still
        // advertises the OLD cron while a NEW live job would otherwise keep
        // firing against it until restart (mirror of add_job's rollback).
        if let Err(e) =
            schedules::update_schedule(&self.db, job_id, cron, enabled.map(|b| b as i32)).await
        {
            // Revert the live registration to match the still-persisted row.
            // Only re-register when the job was previously ENABLED — the old
            // code keyed this on `new_enabled`, so a failed enable of a
            // previously-disabled job left a live cron firing a row that
            // still says `enabled = 0`.
            self.unregister_cron_job(job_id).await;
            if current.enabled != 0 {
                let _ = self.register_cron_job(job_id, &current.cron).await;
            }
            return Err(SchedulerError::from(e));
        }
        Ok(())
    }

    /// Delete a schedule row *and* unregister its live cron job — without
    /// this, a deleted job's cron trigger keeps firing (as a harmless no-op,
    /// since `execute_job` re-fetches the row and finds it gone, but it
    /// never stops trying, leaking a registration for the daemon's lifetime).
    pub async fn remove_job(&mut self, job_id: &str) -> Result<u64, SchedulerError> {
        self.unregister_cron_job(job_id).await;
        schedules::delete_schedule(&self.db, job_id)
            .await
            .map_err(SchedulerError::from)
    }

    /// Create a schedule row and (if enabled) register its cron job
    /// immediately, without requiring a scheduler restart.
    pub async fn add_job(
        &mut self,
        name: &str,
        cron: &str,
        recipe_id: &str,
        enabled: bool,
    ) -> Result<String, SchedulerError> {
        // Full UUIDs, not the old 8-char truncation: at 8 hex chars a
        // collision was realistic, and a colliding id would overwrite the
        // existing job's live registration mapping in `register_cron_job`
        // (and then unregister THAT job on rollback). The existence check
        // below is the belt-and-suspenders half of the same fix.
        let id = uuid::Uuid::new_v4().to_string();
        if schedules::get_schedule(&self.db, &id)
            .await
            .map_err(SchedulerError::from)?
            .is_some()
        {
            return Err(SchedulerError::Cron(format!(
                "schedule id collision, retry: {id}"
            )));
        }
        // Validate + register the live cron BEFORE persisting, so an invalid
        // cron returns an error with no half-written DB row. Under
        // `enabled=false` there's nothing to register.
        if enabled {
            self.register_cron_job(&id, cron).await?;
        }
        if let Err(e) =
            schedules::create_schedule(&self.db, &id, name, cron, recipe_id, enabled as i32).await
        {
            // Roll back the live registration — the DB insert failed, so a
            // live job with no row would fire forever against nothing.
            self.unregister_cron_job(&id).await;
            return Err(SchedulerError::from(e));
        }
        Ok(id)
    }

    /// Execute one scheduled job: temp session + `execution_history`
    /// bookkeeping, then the recipe run, using this scheduler's own DB +
    /// engine handles. Returns `false` (not an error) when the job is
    /// genuinely missing, so a caller can distinguish 404 from a real
    /// storage failure (500). Used by `tests/scheduler_and_recipes.rs`.
    pub async fn run_job(&self, job_id: &str) -> Result<bool, SchedulerError> {
        let job = schedules::get_schedule(&self.db, job_id)
            .await
            .map_err(SchedulerError::from)?;
        let Some(job) = job else {
            return Ok(false);
        };
        execute_job(&self.db, &self.engine, &job.id).await;
        Ok(true)
    }

    pub async fn stop(&mut self) {
        if let Err(e) = self.inner.shutdown().await {
            tracing::warn!("Scheduler shutdown error: {e}");
        }
        tracing::info!("Scheduler stopped");
    }
}

/// Execute one scheduled job: temp session + `execution_history` bookkeeping,
/// then the recipe run. `pub(crate)` so the `run_now` route can execute a job
/// using its own `db`/`recipe_engine` handles WITHOUT holding the scheduler
/// mutex — running a multi-minute recipe turn while holding it serialized
/// every other `POST/PATCH/DELETE /api/schedules*` call behind the one job.
/// Ported exactly from Python's `_execute_job`,
/// including the asymmetry between the success and failure paths — see the
/// comment below for why the failure path can't just delete the temp
/// session the way the success path does.
pub(crate) async fn execute_job(db: &SqlitePool, engine: &RecipeEngine, job_id: &str) {
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

    // Never run a job whose row says `enabled = 0`. The live cron is
    // (un)registered to match the row, but the two can diverge (an
    // unregister that only partially failed, a rollback window in
    // `update_job`) — and a disabled job must not run regardless of what
    // fired. This also covers `run_now`: a manual trigger of a disabled
    // schedule is a no-op rather than a surprise run.
    if job.enabled == 0 {
        tracing::debug!("scheduled job {job_id}: skipping, schedule is disabled");
        return;
    }

    let exec_id = uuid::Uuid::new_v4().simple().to_string();
    let temp_sid = format!("_job_{exec_id}");
    if let Err(e) = sessions::create_session(db, &temp_sid, &format!("scheduled:{job_id}")).await {
        // Previously `is_err() { return }` — a create failure (e.g. a rare
        // exec_id collision on the PK) silently dropped the whole tick with
        // no trace.
        tracing::error!("scheduled job {job_id}: failed to create temp session: {e}");
        return;
    }
    let _ = sessions::update_session_status(db, &temp_sid, "idle").await;
    if let Err(e) =
        execution::insert_execution(db, &exec_id, &temp_sid, "schedule", Some(job_id)).await
    {
        tracing::error!("scheduled job {job_id}: failed to insert execution row: {e}");
        let _ = sessions::delete_session(db, &temp_sid).await;
        return;
    }

    let parameters: Value = job
        .parameters
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| json!({}));

    match engine.execute(&job.recipe_id, parameters).await {
        Ok(session_id) => {
            if let Err(e) = sqlx::query(
                "UPDATE execution_history SET status = 'completed', session_id = ?, completed_at = CURRENT_TIMESTAMP WHERE id = ?",
            )
            .bind(&session_id)
            .bind(&exec_id)
            .execute(db)
            .await
            {
                // A failed completion-update leaves the execution_history row
                // `running` forever — previously silent. Still clean up the
                // temp session (removing it is safe: it isn't referenced by
                // any history row that matters now), but tell the operator.
                tracing::error!(
                    "scheduled job {job_id}: failed to mark execution {exec_id} completed: {e}"
                );
            }
            let _ = sessions::delete_session(db, &temp_sid).await;
        }
        Err(e) => {
            tracing::error!("Scheduled job {job_id} failed: {e}");
            // Record the failure on the execution row (audit trail) rather
            // than deleting it: with `run_turn_and_wait` now propagating the
            // turn outcome, this arm also fires for provider-failed turns,
            // and those must be visible as `failed` — previously every such
            // run was misrecorded as `completed`.
            //
            // `session_id` is nulled in the same statement (migration 016
            // made the column nullable) so the row stops anchoring the
            // throwaway `_job_` session. Before that, the failure path had no
            // choice but to keep it, and every failed run leaked a session
            // plus its whole message batch forever.
            if let Err(e2) = sqlx::query(
                "UPDATE execution_history SET status = 'failed', session_id = NULL, error_message = ?, completed_at = CURRENT_TIMESTAMP WHERE id = ?",
            )
            .bind(e.to_string())
            .bind(&exec_id)
            .execute(db)
            .await
            {
                tracing::error!(
                    "scheduled job {job_id}: failed to mark execution {exec_id} failed: {e2}"
                );
                // The row still points at the temp session, so deleting it
                // would violate the FK. Leave both; the retention sweep in
                // `storage` prunes the pair once the row ages out.
                return;
            }
            // Messages cascade with the session (`messages.session_id` is
            // ON DELETE CASCADE).
            if let Err(e2) = sessions::delete_session(db, &temp_sid).await {
                tracing::warn!(
                    "scheduled job {job_id}: failed to delete temp session {temp_sid}: {e2}"
                );
            }
        }
    }
}
