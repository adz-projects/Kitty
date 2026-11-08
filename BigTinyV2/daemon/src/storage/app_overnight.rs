//! Per-app overnight-learning settings and last-run record (`app_overnight`).
//! See `migrations/030_app_overnight.sql` and `crate::overnight`.

use sqlx::SqlitePool;

use crate::error::StorageError;

/// The stored row.
#[derive(Debug, Clone, Default, sqlx::FromRow)]
pub struct Row {
    pub config: String,
    pub last_started: Option<String>,
    pub last_finished: Option<String>,
    pub last_report: Option<String>,
}

pub async fn get(pool: &SqlitePool, app_id: &str) -> Result<Option<Row>, StorageError> {
    Ok(sqlx::query_as::<_, Row>(
        "SELECT config, last_started, last_finished, last_report FROM app_overnight WHERE app_id = ?",
    )
    .bind(app_id)
    .fetch_optional(pool)
    .await?)
}

/// Replace the app's settings, keeping its run record.
pub async fn set_config(pool: &SqlitePool, app_id: &str, config: &str) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO app_overnight (app_id, config, updated_at) VALUES (?, ?, datetime('now')) \
         ON CONFLICT(app_id) DO UPDATE SET config = excluded.config, updated_at = excluded.updated_at",
    )
    .bind(app_id)
    .bind(config)
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove the app's settings and record. Returns whether there were any.
pub async fn clear(pool: &SqlitePool, app_id: &str) -> Result<bool, StorageError> {
    let r = sqlx::query("DELETE FROM app_overnight WHERE app_id = ?")
        .bind(app_id)
        .execute(pool)
        .await?;
    Ok(r.rows_affected() > 0)
}

/// Record that a pass started (only for an app with settings).
pub async fn mark_started(pool: &SqlitePool, app_id: &str, at: &str) -> Result<(), StorageError> {
    sqlx::query("UPDATE app_overnight SET last_started = ? WHERE app_id = ?")
        .bind(at)
        .bind(app_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record a pass's report; `finished_at` only when it ran to completion.
pub async fn record_report(
    pool: &SqlitePool,
    app_id: &str,
    report: &str,
    finished_at: Option<&str>,
) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE app_overnight SET last_report = ?, \
         last_finished = COALESCE(?, last_finished) WHERE app_id = ?",
    )
    .bind(report)
    .bind(finished_at)
    .bind(app_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn settings_and_record_round_trip() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        assert!(get(&pool, "kitty").await.unwrap().is_none());
        set_config(&pool, "kitty", "{\"enabled\":true}").await.unwrap();
        mark_started(&pool, "kitty", "2026-11-07T01:00:00Z").await.unwrap();
        record_report(&pool, "kitty", "{}", Some("2026-11-07T02:00:00Z")).await.unwrap();
        record_report(&pool, "kitty", "{\"x\":1}", None).await.unwrap();
        set_config(&pool, "kitty", "{\"enabled\":false}").await.unwrap();
        let row = get(&pool, "kitty").await.unwrap().unwrap();
        assert_eq!(row.config, "{\"enabled\":false}");
        assert_eq!(row.last_finished.as_deref(), Some("2026-11-07T02:00:00Z"));
        assert_eq!(row.last_report.as_deref(), Some("{\"x\":1}"));
        assert!(clear(&pool, "kitty").await.unwrap());
    }
}
