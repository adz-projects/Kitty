use chrono::DateTime;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};

use crate::error::StorageError;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ProviderRow {
    pub id: String,
    pub name: String,
    pub provider_type: String,
    pub base_url: String,
    pub fallback_priority: i32,
    pub config: Option<String>,
    pub status: String,
    pub error_message: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

pub async fn list_providers(pool: &SqlitePool) -> Result<Vec<ProviderRow>, StorageError> {
    let rows = sqlx::query_as::<_, ProviderRow>(
        r#"SELECT id, name, provider_type, base_url, fallback_priority, config, status, error_message, created_at, updated_at
           FROM providers ORDER BY fallback_priority ASC"#
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get_provider(
    pool: &SqlitePool,
    provider_id: &str,
) -> Result<Option<ProviderRow>, StorageError> {
    let row = sqlx::query_as::<_, ProviderRow>(
        r#"SELECT id, name, provider_type, base_url, fallback_priority, config, status, error_message, created_at, updated_at
           FROM providers WHERE id = ?"#
    )
    .bind(provider_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn create_provider(
    pool: &SqlitePool,
    id: &str,
    name: &str,
    provider_type: &str,
    base_url: &str,
) -> Result<ProviderRow, StorageError> {
    sqlx::query(r#"INSERT INTO providers (id, name, provider_type, base_url) VALUES (?, ?, ?, ?)"#)
        .bind(id)
        .bind(name)
        .bind(provider_type)
        .bind(base_url)
        .execute(pool)
        .await?;
    get_provider(pool, id)
        .await?
        .ok_or_else(|| StorageError::Generic(format!("Provider {} not found after creation", id)))
}

pub async fn update_provider(
    pool: &SqlitePool,
    provider_id: &str,
    name: Option<&str>,
    base_url: Option<&str>,
    config: Option<&str>,
) -> Result<(), StorageError> {
    sqlx::query(
        r#"UPDATE providers SET name = COALESCE(?1, name), base_url = COALESCE(?2, base_url),
           config = COALESCE(?3, config), updated_at = CURRENT_TIMESTAMP WHERE id = ?4"#,
    )
    .bind(name)
    .bind(base_url)
    .bind(config)
    .bind(provider_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_provider(pool: &SqlitePool, provider_id: &str) -> Result<u64, StorageError> {
    let result = sqlx::query(r#"DELETE FROM providers WHERE id = ?"#)
        .bind(provider_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

pub async fn update_provider_status(
    pool: &SqlitePool,
    provider_id: &str,
    status: &str,
    error_message: Option<&str>,
) -> Result<(), StorageError> {
    sqlx::query(
        r#"UPDATE providers SET status = ?, error_message = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?"#
    )
    .bind(status)
    .bind(error_message)
    .bind(provider_id)
    .execute(pool)
    .await?;
    Ok(())
}
