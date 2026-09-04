use chrono::DateTime;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};

use crate::error::StorageError;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct MCPServerRow {
    pub id: String,
    pub name: String,
    pub transport: String,
    pub command: Option<String>,
    pub args: Option<String>,
    pub url: Option<String>,
    pub env: Option<String>,
    pub headers: Option<String>,
    pub enabled: i32,
    /// Per-server tool-call timeout in seconds; `None` falls back to
    /// `mcp::manager::DEFAULT_TOOL_TIMEOUT`.
    pub timeout_s: Option<i64>,
    pub status: String,
    pub error_message: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    /// Owning app, or `None` for a **shared** server every app can use.
    ///
    /// Same model as providers: shared is expressible so a genuinely
    /// machine-wide tool server need not be registered once per frontend,
    /// but a shared row is mutable by nobody through the API -- reconfiguring
    /// the command every app's agent loop is executing is not one client's
    /// call.
    pub app_id: Option<String>,
}


// ---------------------------------------------------------------------------
// Tenancy
//
// Visible = own row OR shared (`app_id IS NULL`). Mutable = own row only.
// See `MCPServerRow::app_id`.
// ---------------------------------------------------------------------------

/// Servers this app may use: its own, plus shared ones.
pub async fn list_servers_for_app(
    pool: &SqlitePool,
    app_id: &str,
) -> Result<Vec<MCPServerRow>, StorageError> {
    let sql = format!(
        "SELECT id, name, transport, command, args, url, env, headers, enabled, timeout_s, status, error_message, created_at, updated_at, app_id FROM mcp_servers WHERE (app_id = ?1 OR app_id IS NULL) ORDER BY name"
    );
    let rows = sqlx::query_as::<_, MCPServerRow>(&sql)
        .bind(app_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// One server, if this app may see it.
pub async fn get_server_for_app(
    pool: &SqlitePool,
    server_id: &str,
    app_id: &str,
) -> Result<Option<MCPServerRow>, StorageError> {
    let sql = format!(
        "SELECT id, name, transport, command, args, url, env, headers, enabled, timeout_s, status, error_message, created_at, updated_at, app_id FROM mcp_servers WHERE id = ?2 AND (app_id = ?1 OR app_id IS NULL)"
    );
    let row = sqlx::query_as::<_, MCPServerRow>(&sql)
        .bind(app_id)
        .bind(server_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Delete only a row this app owns. Shared servers are not deletable through
/// the API, for the same reason they are not editable.
pub async fn delete_server_owned(
    pool: &SqlitePool,
    server_id: &str,
    app_id: &str,
) -> Result<u64, StorageError> {
    let result = sqlx::query("DELETE FROM mcp_servers WHERE id = ? AND app_id = ?")
        .bind(server_id)
        .bind(app_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// The owning app for a server, if any. `Ok(None)` covers both "shared" and
/// "no such server"; callers that need to tell those apart should use
/// [`get_server_for_app`].
pub async fn owner_of(
    pool: &SqlitePool,
    server_id: &str,
) -> Result<Option<String>, StorageError> {
    let owner: Option<Option<String>> =
        sqlx::query_scalar("SELECT app_id FROM mcp_servers WHERE id = ?")
            .bind(server_id)
            .fetch_optional(pool)
            .await?;
    Ok(owner.flatten())
}

pub async fn list_servers(pool: &SqlitePool) -> Result<Vec<MCPServerRow>, StorageError> {
    let rows = sqlx::query_as::<_, MCPServerRow>(
        r#"SELECT id, name, transport, command, args, url, env, headers, enabled, timeout_s, status, error_message, created_at, updated_at, app_id
           FROM mcp_servers ORDER BY name ASC"#
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get_server(
    pool: &SqlitePool,
    server_id: &str,
) -> Result<Option<MCPServerRow>, StorageError> {
    let row = sqlx::query_as::<_, MCPServerRow>(
        r#"SELECT id, name, transport, command, args, url, env, headers, enabled, timeout_s, status, error_message, created_at, updated_at, app_id
           FROM mcp_servers WHERE id = ?"#
    )
    .bind(server_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// `app_id: None` registers a **shared** server every app can use.
pub async fn create_server(
    pool: &SqlitePool,
    id: &str,
    name: &str,
    transport: &str,
    app_id: Option<&str>,
) -> Result<(), StorageError> {
    sqlx::query(r#"INSERT INTO mcp_servers (id, name, transport, app_id) VALUES (?, ?, ?, ?)"#)
        .bind(id)
        .bind(name)
        .bind(transport)
        .bind(app_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn update_server(
    pool: &SqlitePool,
    server_id: &str,
    name: Option<&str>,
    transport: Option<&str>,
    url: Option<&str>,
    enabled: Option<i32>,
) -> Result<(), StorageError> {
    sqlx::query(
        r#"UPDATE mcp_servers SET
           name = COALESCE(?1, name),
           transport = COALESCE(?2, transport),
           url = COALESCE(?3, url),
           enabled = COALESCE(?4, enabled),
           updated_at = CURRENT_TIMESTAMP
           WHERE id = ?5"#,
    )
    .bind(name)
    .bind(transport)
    .bind(url)
    .bind(enabled)
    .bind(server_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_status(
    pool: &SqlitePool,
    server_id: &str,
    status: &str,
    error_message: Option<&str>,
) -> Result<(), StorageError> {
    sqlx::query(
        r#"UPDATE mcp_servers SET status = ?1, error_message = ?2, updated_at = CURRENT_TIMESTAMP WHERE id = ?3"#
    )
    .bind(status)
    .bind(error_message)
    .bind(server_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_server(pool: &SqlitePool, server_id: &str) -> Result<u64, StorageError> {
    let result = sqlx::query(r#"DELETE FROM mcp_servers WHERE id = ?"#)
        .bind(server_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}
