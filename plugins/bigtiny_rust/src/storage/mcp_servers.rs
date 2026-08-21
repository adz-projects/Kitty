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
}

pub async fn list_servers(pool: &SqlitePool) -> Result<Vec<MCPServerRow>, StorageError> {
    let rows = sqlx::query_as::<_, MCPServerRow>(
        r#"SELECT id, name, transport, command, args, url, env, headers, enabled, timeout_s, status, error_message, created_at, updated_at
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
        r#"SELECT id, name, transport, command, args, url, env, headers, enabled, timeout_s, status, error_message, created_at, updated_at
           FROM mcp_servers WHERE id = ?"#
    )
    .bind(server_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn create_server(
    pool: &SqlitePool,
    id: &str,
    name: &str,
    transport: &str,
) -> Result<(), StorageError> {
    sqlx::query(r#"INSERT INTO mcp_servers (id, name, transport) VALUES (?, ?, ?)"#)
        .bind(id)
        .bind(name)
        .bind(transport)
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
