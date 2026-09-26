use chrono::DateTime;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};

use crate::error::StorageError;

/// How a schedule decides when to run.
pub const KIND_CRON: &str = "cron";
pub const KIND_INTERVAL: &str = "interval";
pub const KIND_ONCE: &str = "once";

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ScheduleRow {
    pub id: String,
    pub name: String,
    /// Standard 5-field crontab, for `kind = "cron"`; `''` for the timer kinds.
    pub cron: String,
    /// The prompt a firing sends as an ordinary turn.
    ///
    /// Replaced `recipe_id`/`parameters` when recipes became specialists: a
    /// scheduled run is now a plain turn whose model may itself delegate, so
    /// the scheduler no longer needs a concept of a pre-rendered task.
    pub prompt: String,
    pub enabled: i32,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    /// Owning app. `NOT NULL`: a schedule runs a turn on one app's behalf,
    /// against its provider and its billing account.
    pub app_id: String,
    /// `cron` | `interval` | `once`. See migration 025.
    pub kind: String,
    /// For `interval`: seconds between runs.
    pub interval_secs: Option<i64>,
    /// For `once`: when to run (RFC 3339).
    pub run_at: Option<String>,
    /// For the timer kinds: when the next run is due (RFC 3339). Persisted so
    /// a run that fell due while the daemon was down is caught up on start.
    pub next_run_at: Option<String>,
    /// Pin runs to this provider/model, as a session's own pin would.
    pub provider_id: Option<String>,
    pub model: Option<String>,
    /// Becomes the run session's persona.
    pub system_prompt: Option<String>,
    /// How long a run waits for a tool approval before continuing without it.
    pub hitl_timeout_secs: i64,
    pub last_run_at: Option<String>,
    /// `running` | `completed` | `completed_with_denied_tools` | `failed`.
    pub last_status: Option<String>,
    pub last_session_id: Option<String>,
}

const COLUMNS: &str = "id, name, cron, prompt, enabled, created_at, updated_at, app_id, kind, \
    interval_secs, run_at, next_run_at, provider_id, model, system_prompt, hitl_timeout_secs, \
    last_run_at, last_status, last_session_id";

/// Everything that defines a schedule, as a client sends it.
#[derive(Debug, Clone, Default)]
pub struct ScheduleSpec {
    pub name: String,
    pub prompt: String,
    pub kind: String,
    pub cron: String,
    pub interval_secs: Option<i64>,
    pub run_at: Option<String>,
    pub next_run_at: Option<String>,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub hitl_timeout_secs: i64,
    pub enabled: bool,
}

impl ScheduleSpec {
    /// The spec a stored row was created from, for merging an edit into.
    pub fn from_row(row: &ScheduleRow) -> Self {
        Self {
            name: row.name.clone(),
            prompt: row.prompt.clone(),
            kind: row.kind.clone(),
            cron: row.cron.clone(),
            interval_secs: row.interval_secs,
            run_at: row.run_at.clone(),
            next_run_at: row.next_run_at.clone(),
            provider_id: row.provider_id.clone(),
            model: row.model.clone(),
            system_prompt: row.system_prompt.clone(),
            hitl_timeout_secs: row.hitl_timeout_secs,
            enabled: row.enabled != 0,
        }
    }
}

/// Schedules belonging to this app.
pub async fn list_schedules_for_app(
    pool: &SqlitePool,
    app_id: &str,
) -> Result<Vec<ScheduleRow>, StorageError> {
    let sql = format!("SELECT {COLUMNS} FROM schedule_jobs WHERE app_id = ? ORDER BY name");
    let rows = sqlx::query_as::<_, ScheduleRow>(&sql)
        .bind(app_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// One schedule, if this app owns it.
pub async fn get_schedule_for_app(
    pool: &SqlitePool,
    schedule_id: &str,
    app_id: &str,
) -> Result<Option<ScheduleRow>, StorageError> {
    let sql = format!("SELECT {COLUMNS} FROM schedule_jobs WHERE id = ? AND app_id = ?");
    let row = sqlx::query_as::<_, ScheduleRow>(&sql)
        .bind(schedule_id)
        .bind(app_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Delete only a schedule this app owns.
pub async fn delete_schedule_for_app(
    pool: &SqlitePool,
    schedule_id: &str,
    app_id: &str,
) -> Result<u64, StorageError> {
    let result = sqlx::query("DELETE FROM schedule_jobs WHERE id = ? AND app_id = ?")
        .bind(schedule_id)
        .bind(app_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

pub async fn list_schedules(pool: &SqlitePool) -> Result<Vec<ScheduleRow>, StorageError> {
    let sql = format!("SELECT {COLUMNS} FROM schedule_jobs ORDER BY name ASC");
    let rows = sqlx::query_as::<_, ScheduleRow>(&sql).fetch_all(pool).await?;
    Ok(rows)
}

pub async fn get_schedule(
    pool: &SqlitePool,
    schedule_id: &str,
) -> Result<Option<ScheduleRow>, StorageError> {
    let sql = format!("SELECT {COLUMNS} FROM schedule_jobs WHERE id = ?");
    let row = sqlx::query_as::<_, ScheduleRow>(&sql)
        .bind(schedule_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Create a plain cron schedule. Kept for existing callers; new code uses
/// [`create_schedule_spec`].
pub async fn create_schedule(
    pool: &SqlitePool,
    id: &str,
    name: &str,
    cron: &str,
    prompt: &str,
    enabled: i32,
    app_id: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        r#"INSERT INTO schedule_jobs (id, name, cron, prompt, enabled, app_id)
           VALUES (?, ?, ?, ?, ?, ?)"#,
    )
    .bind(id)
    .bind(name)
    .bind(cron)
    .bind(prompt)
    .bind(enabled)
    .bind(app_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Create a schedule of any kind from its full spec.
pub async fn create_schedule_spec(
    pool: &SqlitePool,
    id: &str,
    app_id: &str,
    spec: &ScheduleSpec,
) -> Result<(), StorageError> {
    sqlx::query(
        r#"INSERT INTO schedule_jobs (id, name, cron, prompt, enabled, app_id, kind, interval_secs,
               run_at, next_run_at, provider_id, model, system_prompt, hitl_timeout_secs)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(id)
    .bind(&spec.name)
    .bind(&spec.cron)
    .bind(&spec.prompt)
    .bind(spec.enabled as i32)
    .bind(app_id)
    .bind(&spec.kind)
    .bind(spec.interval_secs)
    .bind(&spec.run_at)
    .bind(&spec.next_run_at)
    .bind(&spec.provider_id)
    .bind(&spec.model)
    .bind(&spec.system_prompt)
    .bind(spec.hitl_timeout_secs)
    .execute(pool)
    .await?;
    Ok(())
}

/// Replace every client-editable field of a schedule.
pub async fn update_schedule_spec(
    pool: &SqlitePool,
    schedule_id: &str,
    spec: &ScheduleSpec,
) -> Result<(), StorageError> {
    sqlx::query(
        r#"UPDATE schedule_jobs SET
           name = ?, cron = ?, prompt = ?, enabled = ?, kind = ?, interval_secs = ?,
           run_at = ?, next_run_at = ?, provider_id = ?, model = ?, system_prompt = ?,
           hitl_timeout_secs = ?, updated_at = CURRENT_TIMESTAMP
           WHERE id = ?"#,
    )
    .bind(&spec.name)
    .bind(&spec.cron)
    .bind(&spec.prompt)
    .bind(spec.enabled as i32)
    .bind(&spec.kind)
    .bind(spec.interval_secs)
    .bind(&spec.run_at)
    .bind(&spec.next_run_at)
    .bind(&spec.provider_id)
    .bind(&spec.model)
    .bind(&spec.system_prompt)
    .bind(spec.hitl_timeout_secs)
    .bind(schedule_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_schedule(
    pool: &SqlitePool,
    schedule_id: &str,
    cron: Option<&str>,
    enabled: Option<i32>,
) -> Result<(), StorageError> {
    sqlx::query(
        r#"UPDATE schedule_jobs SET
           cron = COALESCE(?1, cron),
           enabled = COALESCE(?2, enabled),
           updated_at = CURRENT_TIMESTAMP
           WHERE id = ?3"#,
    )
    .bind(cron)
    .bind(enabled)
    .bind(schedule_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record when a timer schedule is next due (and, with `enabled`, whether it
/// still is at all -- a `once` schedule disables itself after running).
pub async fn set_next_run(
    pool: &SqlitePool,
    schedule_id: &str,
    next_run_at: Option<&str>,
    enabled: bool,
) -> Result<(), StorageError> {
    sqlx::query("UPDATE schedule_jobs SET next_run_at = ?, enabled = ? WHERE id = ?")
        .bind(next_run_at)
        .bind(enabled as i32)
        .bind(schedule_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record the latest run's outcome on the schedule row itself.
pub async fn record_last_run(
    pool: &SqlitePool,
    schedule_id: &str,
    status: &str,
    session_id: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE schedule_jobs SET last_run_at = ?, last_status = ?, last_session_id = ? WHERE id = ?",
    )
    .bind(Utc::now().to_rfc3339())
    .bind(status)
    .bind(session_id)
    .bind(schedule_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_schedule(pool: &SqlitePool, schedule_id: &str) -> Result<u64, StorageError> {
    let result = sqlx::query(r#"DELETE FROM schedule_jobs WHERE id = ?"#)
        .bind(schedule_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}
