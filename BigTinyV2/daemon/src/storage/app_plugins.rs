//! Per-app plugin selection (`app_plugins`).
//!
//! See `migrations/018_app_plugins.sql` for why plugins get their own table
//! rather than sharing one with MCP servers. The short version: a plugin hooks
//! the agent loop and carries per-app instance state; an MCP server only
//! provides tools. `kitty-tools` is stateful and is still only an MCP server,
//! so the axis is loop integration, not statefulness.

use sqlx::{Row, SqlitePool};

use crate::error::StorageError;

/// The behavioral-memory plugin. The only one today, named as a constant so
/// the string is not scattered across the loop, the routes, and the host.
pub const PATHWAY: &str = "pathway";

#[derive(Debug, Clone)]
pub struct PluginRow {
    pub plugin: String,
    pub enabled: bool,
    pub config: Option<String>,
}

/// Whether `app_id` has `plugin` enabled.
///
/// `None` means the app has expressed no preference, and the caller should
/// fall back to the daemon default. That is deliberately distinct from
/// `Some(false)`: "never asked" and "explicitly turned off" want different
/// behaviour the moment a daemon default changes, and collapsing them would
/// silently re-enable a plugin someone had switched off.
pub async fn is_enabled(
    pool: &SqlitePool,
    app_id: &str,
    plugin: &str,
) -> Result<Option<bool>, StorageError> {
    let row: Option<i64> =
        sqlx::query_scalar("SELECT enabled FROM app_plugins WHERE app_id = ? AND plugin = ?")
            .bind(app_id)
            .bind(plugin)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|v| v != 0))
}

/// Every plugin preference this app has expressed.
pub async fn list_for_app(
    pool: &SqlitePool,
    app_id: &str,
) -> Result<Vec<PluginRow>, StorageError> {
    let rows = sqlx::query(
        "SELECT plugin, enabled, config FROM app_plugins WHERE app_id = ? ORDER BY plugin",
    )
    .bind(app_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| PluginRow {
            plugin: r.get("plugin"),
            enabled: r.get::<i64, _>("enabled") != 0,
            config: r.get("config"),
        })
        .collect())
}

/// Set (or clear) this app's preference for `plugin`.
pub async fn set_enabled(
    pool: &SqlitePool,
    app_id: &str,
    plugin: &str,
    enabled: bool,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO app_plugins (app_id, plugin, enabled) VALUES (?, ?, ?) \
         ON CONFLICT(app_id, plugin) DO UPDATE SET enabled = excluded.enabled, \
         updated_at = datetime('now')",
    )
    .bind(app_id)
    .bind(plugin)
    .bind(enabled as i64)
    .execute(pool)
    .await?;
    Ok(())
}

/// Drop an app's preference, returning it to the daemon default.
pub async fn clear(pool: &SqlitePool, app_id: &str, plugin: &str) -> Result<u64, StorageError> {
    let result = sqlx::query("DELETE FROM app_plugins WHERE app_id = ? AND plugin = ?")
        .bind(app_id)
        .bind(plugin)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        crate::storage::apps::register_app(&pool, "app-a", "A", "key-a")
            .await
            .unwrap();
        pool
    }

    #[tokio::test]
    async fn no_preference_is_distinct_from_disabled() {
        // The distinction that stops a daemon-default change from silently
        // re-enabling a plugin someone deliberately switched off.
        let pool = pool().await;
        assert_eq!(is_enabled(&pool, "app-a", PATHWAY).await.unwrap(), None);

        set_enabled(&pool, "app-a", PATHWAY, false).await.unwrap();
        assert_eq!(is_enabled(&pool, "app-a", PATHWAY).await.unwrap(), Some(false));

        clear(&pool, "app-a", PATHWAY).await.unwrap();
        assert_eq!(is_enabled(&pool, "app-a", PATHWAY).await.unwrap(), None);
    }

    #[tokio::test]
    async fn setting_is_idempotent_and_updates_in_place() {
        let pool = pool().await;
        set_enabled(&pool, "app-a", PATHWAY, true).await.unwrap();
        set_enabled(&pool, "app-a", PATHWAY, false).await.unwrap();
        assert_eq!(is_enabled(&pool, "app-a", PATHWAY).await.unwrap(), Some(false));
        assert_eq!(list_for_app(&pool, "app-a").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn two_apps_hold_independent_preferences() {
        let pool = pool().await;
        crate::storage::apps::register_app(&pool, "app-b", "B", "key-b")
            .await
            .unwrap();

        set_enabled(&pool, "app-a", PATHWAY, true).await.unwrap();
        set_enabled(&pool, "app-b", PATHWAY, false).await.unwrap();

        assert_eq!(is_enabled(&pool, "app-a", PATHWAY).await.unwrap(), Some(true));
        assert_eq!(is_enabled(&pool, "app-b", PATHWAY).await.unwrap(), Some(false));
        assert!(list_for_app(&pool, "app-a").await.unwrap()[0].enabled);
    }
}
