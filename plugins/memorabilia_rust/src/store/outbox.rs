//! Durable async reinforcement outbox rows + SQL primitives (plan §6.2).
//! Rows are enqueued in the grounding transaction; a background worker
//! drains FIFO batches, applies the counters, and deletes the rows.
//! `UNIQUE(chunk_id, query_event_id)` is what makes at-least-once delivery
//! idempotent — a redelivered pair is a rejected no-op, not a double
//! increment.

use sqlx::FromRow;

use crate::error::Result;
use crate::store::Db;

#[derive(Debug, Clone, FromRow)]
pub struct OutboxEntry {
    pub chunk_id: String,
    pub query_event_id: String,
    /// Every payload chunk is grounded (audit, plan §6.1).
    pub grounded: bool,
    /// Grounded AND no open DISPUTED edge — resolved at write time
    /// (plan §6.1): disputed chunks are never reinforced.
    pub reinforced: bool,
    pub created_at: String,
}

impl Db {
    /// Enqueue a deduplicated row; a repeat of the same (chunk, event) pair
    /// is a no-op (at-least-once crash replay).
    pub async fn enqueue_outbox(&self, e: &OutboxEntry) -> Result<()> {
        sqlx::query(
            "INSERT INTO reinforcement_outbox (
                chunk_id, query_event_id, grounded, reinforced, created_at
             ) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (chunk_id, query_event_id) DO NOTHING",
        )
        .bind(&e.chunk_id)
        .bind(&e.query_event_id)
        .bind(e.grounded)
        .bind(e.reinforced)
        .bind(&e.created_at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Next FIFO batch (oldest first on rowid), for the drain worker.
    pub async fn next_outbox_batch(&self, limit: i64) -> Result<Vec<OutboxEntry>> {
        Ok(sqlx::query_as::<_, OutboxEntry>(
            "SELECT * FROM reinforcement_outbox ORDER BY rowid LIMIT ?",
        )
        .bind(limit)
        .fetch_all(self.pool())
        .await?)
    }

    /// Delete a drained batch. One statement (no nested transaction —
    /// the pool is single-connection, so this must compose with the drain
    /// worker's own `run_in_transaction` without opening a second BEGIN).
    pub async fn delete_outbox_pairs(
        &self,
        query_event_id: &str,
        chunk_ids: &[String],
    ) -> Result<()> {
        if chunk_ids.is_empty() {
            return Ok(());
        }
        let placeholders = vec!["?"; chunk_ids.len()].join(", ");
        let sql = format!(
            "DELETE FROM reinforcement_outbox WHERE query_event_id = ? AND chunk_id IN ({placeholders})"
        );
        let mut q = sqlx::query(&sql).bind(query_event_id);
        for chunk_id in chunk_ids {
            q = q.bind(chunk_id);
        }
        q.execute(self.pool()).await?;
        Ok(())
    }

    pub async fn outbox_count(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM reinforcement_outbox")
            .fetch_one(self.pool())
            .await?)
    }
}
