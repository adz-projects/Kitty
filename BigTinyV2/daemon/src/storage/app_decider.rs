//! Per-app decision-model configuration (`app_decider`). See
//! `migrations/029_app_decider.sql` and `crate::decider`.

use sqlx::SqlitePool;

use crate::error::StorageError;

/// The app's stored configuration document, if it has one.
pub async fn get(pool: &SqlitePool, app_id: &str) -> Result<Option<String>, StorageError> {
    Ok(
        sqlx::query_scalar("SELECT config FROM app_decider WHERE app_id = ?")
            .bind(app_id)
            .fetch_optional(pool)
            .await?,
    )
}

/// Replace the app's configuration document.
pub async fn set(pool: &SqlitePool, app_id: &str, config: &str) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO app_decider (app_id, config, updated_at) VALUES (?, ?, datetime('now')) \
         ON CONFLICT(app_id) DO UPDATE SET config = excluded.config, updated_at = excluded.updated_at",
    )
    .bind(app_id)
    .bind(config)
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove the app's configuration. Returns whether there was one.
pub async fn clear(pool: &SqlitePool, app_id: &str) -> Result<bool, StorageError> {
    let r = sqlx::query("DELETE FROM app_decider WHERE app_id = ?")
        .bind(app_id)
        .execute(pool)
        .await?;
    Ok(r.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn set_replaces_and_clear_removes() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        assert!(get(&pool, "kitty").await.unwrap().is_none());
        set(&pool, "kitty", "{\"a\":1}").await.unwrap();
        set(&pool, "kitty", "{\"a\":2}").await.unwrap();
        assert_eq!(get(&pool, "kitty").await.unwrap().as_deref(), Some("{\"a\":2}"));
        assert!(get(&pool, "notebook").await.unwrap().is_none());
        assert!(clear(&pool, "kitty").await.unwrap());
        assert!(!clear(&pool, "kitty").await.unwrap());
    }
}
