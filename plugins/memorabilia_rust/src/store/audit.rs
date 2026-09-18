//! Audit log rows + SQL primitives (plan §12.1). PII rejections and
//! forget events are recorded with REASON AND CATEGORY ONLY — never the
//! PII value (claude.md principle 6): `event = "rejected:pii"`,
//! `category = "email"`, `detail` at most a structural description.

use sqlx::FromRow;

use crate::error::Result;
use crate::store::Db;

#[derive(Debug, Clone, FromRow)]
pub struct AuditEntry {
    pub id: String,
    /// e.g. "rejected:pii", "deleted:private", "deleted:wrong",
    /// "deleted:outdated", "reinforced", "promoted".
    pub event: String,
    /// PII category only ("email", "phone", …) — never the value.
    pub category: Option<String>,
    pub detail: Option<String>,
    pub created_at: String,
}

impl Db {
    pub async fn insert_audit(&self, a: &AuditEntry) -> Result<()> {
        sqlx::query(
            "INSERT INTO audit_log (id, event, category, detail, created_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&a.id)
        .bind(&a.event)
        .bind(&a.category)
        .bind(&a.detail)
        .bind(&a.created_at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn list_audit_by_event(&self, event: &str) -> Result<Vec<AuditEntry>> {
        Ok(sqlx::query_as::<_, AuditEntry>(
            "SELECT * FROM audit_log WHERE event = ? ORDER BY rowid",
        )
        .bind(event)
        .fetch_all(self.pool())
        .await?)
    }

    pub async fn count_audit(&self, event: &str) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE event = ?")
            .bind(event)
            .fetch_one(self.pool())
            .await?)
    }
}
