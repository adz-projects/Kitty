use chrono::DateTime;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};

use crate::error::StorageError;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ScheduleRow {
    pub id: String,
    pub name: String,
    pub cron: String,
    /// The prompt a cron firing sends as an ordinary turn.
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
}


/// Schedules belonging to this app.
pub async fn list_schedules_for_app(
    pool: &SqlitePool,
    app_id: &str,
) -> Result<Vec<ScheduleRow>, StorageError> {
    let sql = format!("SELECT id, name, cron, prompt, enabled, created_at, updated_at, app_id FROM schedule_jobs WHERE app_id = ? ORDER BY name");
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
    let sql = format!("SELECT id, name, cron, prompt, enabled, created_at, updated_at, app_id FROM schedule_jobs WHERE id = ? AND app_id = ?");
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
    let rows = sqlx::query_as::<_, ScheduleRow>(
        r#"SELECT id, name, cron, prompt, enabled, created_at, updated_at, app_id
           FROM schedule_jobs ORDER BY name ASC"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get_schedule(
    pool: &SqlitePool,
    schedule_id: &str,
) -> Result<Option<ScheduleRow>, StorageError> {
    let row = sqlx::query_as::<_, ScheduleRow>(
        r#"SELECT id, name, cron, prompt, enabled, created_at, updated_at, app_id
           FROM schedule_jobs WHERE id = ?"#,
    )
    .bind(schedule_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

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

pub async fn delete_schedule(pool: &SqlitePool, schedule_id: &str) -> Result<u64, StorageError> {
    let result = sqlx::query(r#"DELETE FROM schedule_jobs WHERE id = ?"#)
        .bind(schedule_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}
