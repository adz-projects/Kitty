//! `conversation_state` table access. Per-session pause flag, exchange
//! counter, last-recall ids, and the learn watermark.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::Db;
use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationState {
    pub session_id: String,
    pub paused: bool,
    pub exchange_count: i64,
    pub last_learned_rowid: i64,
    pub last_recall_ids: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ConversationStateRow {
    pub session_id: String,
    pub paused: bool,
    pub exchange_count: i64,
    pub last_learned_rowid: i64,
    pub last_recall_ids: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Db {
    pub async fn get_state(&self, session_id: &str) -> Result<Option<ConversationState>> {
        let row: Option<ConversationStateRow> = sqlx::query_as(
            "SELECT session_id, paused, exchange_count, last_learned_rowid, last_recall_ids, \
             created_at, updated_at FROM conversation_state WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(|r| ConversationState {
            session_id: r.session_id,
            paused: r.paused,
            exchange_count: r.exchange_count,
            last_learned_rowid: r.last_learned_rowid,
            last_recall_ids: serde_json::from_str(&r.last_recall_ids).unwrap_or_default(),
            created_at: r.created_at,
            updated_at: r.updated_at,
        }))
    }

    pub async fn ensure_state(&self, session_id: &str) -> Result<()> {
        sqlx::query(
            "INSERT OR IGNORE INTO conversation_state (session_id) VALUES (?)",
        )
        .bind(session_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn set_paused(&self, session_id: &str, paused: bool) -> Result<()> {
        sqlx::query(
            "UPDATE conversation_state SET paused = ?, updated_at = ? WHERE session_id = ?",
        )
        .bind(paused)
        .bind(Utc::now())
        .bind(session_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn is_paused(&self, session_id: &str) -> Result<bool> {
        self.ensure_state(session_id).await?;
        let paused: bool =
            sqlx::query_scalar("SELECT paused FROM conversation_state WHERE session_id = ?")
                .bind(session_id)
                .fetch_one(self.pool())
                .await?;
        Ok(paused)
    }

    pub async fn bump_exchange(&self, session_id: &str) -> Result<i64> {
        self.ensure_state(session_id).await?;
        sqlx::query(
            "UPDATE conversation_state SET exchange_count = exchange_count + 1, updated_at = ? \
             WHERE session_id = ?",
        )
        .bind(Utc::now())
        .bind(session_id)
        .execute(self.pool())
        .await?;
        Ok(sqlx::query_scalar("SELECT exchange_count FROM conversation_state WHERE session_id = ?")
            .bind(session_id)
            .fetch_one(self.pool())
            .await?)
    }

    /// Read the learn watermark. Returns 0 when no state row exists.
    pub async fn last_learned_rowid(&self, session_id: &str) -> Result<i64> {
        self.ensure_state(session_id).await?;
        Ok(sqlx::query_scalar(
            "SELECT last_learned_rowid FROM conversation_state WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_one(self.pool())
        .await?)
    }

    /// Write the learn watermark only forward (`MAX()` guard prevents the
    /// compaction rewind hazard -- see the plan's ordering hazard).
    pub async fn advance_learned_rowid(&self, session_id: &str, through: i64) -> Result<()> {
        self.ensure_state(session_id).await?;
        sqlx::query(
            "UPDATE conversation_state SET last_learned_rowid = MAX(last_learned_rowid, ?), \
             updated_at = ? WHERE session_id = ?",
        )
        .bind(through)
        .bind(Utc::now())
        .bind(session_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn set_last_recall_ids(&self, session_id: &str, ids: &[String]) -> Result<()> {
        self.ensure_state(session_id).await?;
        let json = serde_json::to_string(ids).unwrap_or_else(|_| "[]".into());
        sqlx::query(
            "UPDATE conversation_state SET last_recall_ids = ?, updated_at = ? WHERE session_id = ?",
        )
        .bind(json)
        .bind(Utc::now())
        .bind(session_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }
}
