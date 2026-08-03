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
    pub recipe_id: String,
    pub parameters: Option<String>,
    pub enabled: i32,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

pub async fn list_schedules(pool: &SqlitePool) -> Result<Vec<ScheduleRow>, StorageError> {
    let rows = sqlx::query_as::<_, ScheduleRow>(
        r#"SELECT id, name, cron, recipe_id, parameters, enabled, created_at, updated_at
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
        r#"SELECT id, name, cron, recipe_id, parameters, enabled, created_at, updated_at
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
    recipe_id: &str,
    enabled: i32,
) -> Result<(), StorageError> {
    sqlx::query(
        r#"INSERT INTO schedule_jobs (id, name, cron, recipe_id, enabled) VALUES (?, ?, ?, ?, ?)"#,
    )
    .bind(id)
    .bind(name)
    .bind(cron)
    .bind(recipe_id)
    .bind(enabled)
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
