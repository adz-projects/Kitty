//! `audit_log` and `app_settings` table access.

use chrono::Utc;

use super::Db;
use crate::error::Result;

impl Db {
    pub async fn audit(&self, event: &str, detail: Option<&str>) -> Result<()> {
        sqlx::query("INSERT INTO audit_log (id, event, detail, created_at) VALUES (?, ?, ?, ?)")
            .bind(uuid_string())
            .bind(event)
            .bind(detail)
            .bind(Utc::now())
            .execute(self.pool())
            .await?;
        Ok(())
    }

    // ---- app_settings ----

    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT value FROM app_settings WHERE key = ?")
            .bind(key)
            .fetch_optional(self.pool())
            .await?)
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO app_settings (key, value) VALUES (?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(self.pool())
        .await?;
        Ok(())
    }
}

pub fn uuid_string() -> String {
    uuid::Uuid::new_v4().to_string()
}
