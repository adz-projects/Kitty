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

    // ---- global exchange counter ----
    //
    // A single process-wide (not per-session) counter of learn-worthy
    // exchanges, matching the plan's "exchanges_at_flag = <global exchange
    // counter>" language. Assumption scheduling needs *some* monotonic
    // clock to measure "how long has this gone untested" against; re-
    // stamping a per-assumption counter on every maintenance tick (the
    // original approach) ties elapsed-exchanges to how often maintenance
    // happens to run rather than to actual exchange volume, which drifted
    // wildly after the maintenance-cadence fix (issue #1) collapsed ticks
    // from every 60s to roughly nightly. Bumped once per learn pass
    // (`extract_and_record`), which is exactly the granularity assumptions
    // are scheduled against ("~20 exchanges" in the plan means ~20 learn-
    // worthy exchanges, not calendar time).

    const GLOBAL_EXCHANGE_KEY: &str = "global_exchange_count";

    pub async fn global_exchange_count(&self) -> Result<i64> {
        Ok(self
            .get_setting(Self::GLOBAL_EXCHANGE_KEY)
            .await?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    pub async fn bump_global_exchange(&self) -> Result<i64> {
        let next = self.global_exchange_count().await? + 1;
        self.set_setting(Self::GLOBAL_EXCHANGE_KEY, &next.to_string()).await?;
        Ok(next)
    }
}

pub fn uuid_string() -> String {
    uuid::Uuid::new_v4().to_string()
}
