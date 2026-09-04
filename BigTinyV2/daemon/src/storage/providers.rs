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
    /// Owning app, or `None` for the **shared pool** every app can see.
    ///
    /// Shared is expressible on purpose: a user who configures one Anthropic
    /// key should not have to re-enter it once per frontend. Private is the
    /// default for anything an app creates for itself.
    pub app_id: Option<String>,
}


// ---------------------------------------------------------------------------
// Tenancy
//
// A provider is visible to an app when the app owns it OR it is in the shared
// pool (`app_id IS NULL`). Mutation is stricter than visibility: an app may
// *use* a shared provider but may not edit or delete one, because a row every
// app depends on is not one client's to reconfigure.
// ---------------------------------------------------------------------------

/// The SQL fragment for "this app can see this row". Kept in one place so
/// visibility cannot drift between the list, get, and mutate paths.
const VISIBLE: &str = "(app_id = ?1 OR app_id IS NULL)";

/// Providers this app may use: its own, plus the shared pool.
pub async fn list_providers_for_app(
    pool: &SqlitePool,
    app_id: &str,
) -> Result<Vec<ProviderRow>, StorageError> {
    let sql = format!(
        r#"SELECT id, name, provider_type, base_url, fallback_priority, config, status, error_message, created_at, updated_at, app_id FROM providers WHERE {VISIBLE}
           ORDER BY fallback_priority ASC, id ASC"#
    );
    let rows = sqlx::query_as::<_, ProviderRow>(&sql)
        .bind(app_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// One provider, if this app may see it.
pub async fn get_provider_for_app(
    pool: &SqlitePool,
    provider_id: &str,
    app_id: &str,
) -> Result<Option<ProviderRow>, StorageError> {
    let sql = format!(r#"SELECT id, name, provider_type, base_url, fallback_priority, config, status, error_message, created_at, updated_at, app_id FROM providers WHERE id = ?2 AND {VISIBLE}"#);
    let row = sqlx::query_as::<_, ProviderRow>(&sql)
        .bind(app_id)
        .bind(provider_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Create a provider owned by `app_id`, or shared when `app_id` is `None`.
pub async fn create_provider_for_app(
    pool: &SqlitePool,
    id: &str,
    name: &str,
    provider_type: &str,
    base_url: &str,
    app_id: Option<&str>,
) -> Result<ProviderRow, StorageError> {
    sqlx::query(
        r#"INSERT INTO providers (id, name, provider_type, base_url, app_id)
           VALUES (?, ?, ?, ?, ?)"#,
    )
    .bind(id)
    .bind(name)
    .bind(provider_type)
    .bind(base_url)
    .bind(app_id)
    .execute(pool)
    .await?;
    get_provider(pool, id)
        .await?
        .ok_or_else(|| StorageError::Generic(format!("Provider {} not found after creation", id)))
}

/// Update only a row this app **owns**.
///
/// Deliberately narrower than visibility: `app_id = ?` rather than
/// [`VISIBLE`], so an app cannot rewrite the base URL or credentials of a
/// shared provider that every other app is also using. Returns rows affected,
/// so 0 becomes a 404 rather than a false success.
pub async fn update_provider_owned(
    pool: &SqlitePool,
    provider_id: &str,
    app_id: &str,
    name: Option<&str>,
    base_url: Option<&str>,
    config: Option<&str>,
) -> Result<u64, StorageError> {
    let result = sqlx::query(
        r#"UPDATE providers SET name = COALESCE(?1, name), base_url = COALESCE(?2, base_url),
           config = COALESCE(?3, config), updated_at = CURRENT_TIMESTAMP
           WHERE id = ?4 AND app_id = ?5"#,
    )
    .bind(name)
    .bind(base_url)
    .bind(config)
    .bind(provider_id)
    .bind(app_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Delete only a row this app owns. Shared providers are not deletable through
/// the API for the same reason they are not editable.
pub async fn delete_provider_owned(
    pool: &SqlitePool,
    provider_id: &str,
    app_id: &str,
) -> Result<u64, StorageError> {
    let result = sqlx::query(r#"DELETE FROM providers WHERE id = ? AND app_id = ?"#)
        .bind(provider_id)
        .bind(app_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

pub async fn list_providers(pool: &SqlitePool) -> Result<Vec<ProviderRow>, StorageError> {
    let rows = sqlx::query_as::<_, ProviderRow>(
        r#"SELECT id, name, provider_type, base_url, fallback_priority, config, status, error_message, created_at, updated_at, app_id
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
        r#"SELECT id, name, provider_type, base_url, fallback_priority, config, status, error_message, created_at, updated_at, app_id
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
