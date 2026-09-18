//! Forget-ladder rows + SQL primitives (plan §12.2): text-hash tombstones
//! (never relearned) and suppressions (`wrong` / `outdated`). Category-only
//! audit rows live in [`super::audit`].

use sqlx::FromRow;

use crate::error::Result;
use crate::store::Db;

#[derive(Debug, Clone, FromRow)]
pub struct Tombstone {
    /// SHA-256 of the chunk content / document payload / normalized
    /// proposition text — matched at ingestion and extraction
    /// respectively (plan §12.2).
    pub text_hash: String,
    /// "content" | "document" | "proposition".
    pub kind: String,
    /// `private` and `wrong` tombstones never expire.
    pub permanent: bool,
    pub created_at: String,
}

impl Db {
    /// Idempotent (re-forgetting the same text must not fail).
    pub async fn insert_tombstone(&self, t: &Tombstone) -> Result<()> {
        sqlx::query(
            "INSERT INTO tombstones (
                text_hash, kind, permanent, created_at
             ) VALUES (?, ?, ?, ?)
             ON CONFLICT (text_hash) DO NOTHING",
        )
        .bind(&t.text_hash)
        .bind(&t.kind)
        .bind(t.permanent)
        .bind(&t.created_at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// The re-learn gate (plan §12.2): true iff any tombstone matches this
    /// hash. Kind is intentionally not filtered — a content hash hit blocks
    /// re-ingestion regardless of how it was first recorded, and the two
    /// hash spaces (content / proposition) are disjoint by construction.
    pub async fn has_tombstone(&self, text_hash: &str) -> Result<bool> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tombstones WHERE text_hash = ?")
            .bind(text_hash)
            .fetch_one(self.pool())
            .await?;
        Ok(n > 0)
    }

    /// Every tombstone hash (the Stage 4 drain loads the list once per
    /// pass so the per-claim gate is a memory lookup — tombstones are few
    /// and change only on forget events).
    pub async fn list_tombstone_hashes(&self) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT text_hash FROM tombstones ORDER BY rowid",
        )
        .fetch_all(self.pool())
        .await?)
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct Suppression {
    pub chunk_id: String,
    /// "wrong" | "outdated" (CHECK; `private` is a hard delete, not a
    /// suppression — plan §12.2).
    pub reason: String,
    /// `wrong` rows are permanent; `outdated` rows carry `expires_at`.
    pub permanent: bool,
    pub expires_at: Option<String>,
    pub created_at: String,
}

impl Db {
    /// Idempotent per (chunk, reason) — re-forgetting with the same reason
    /// resets the bounds, not duplicates the row.
    pub async fn insert_suppression(&self, s: &Suppression) -> Result<()> {
        sqlx::query(
            "INSERT INTO suppressions (
                chunk_id, reason, permanent, expires_at, created_at
             ) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (chunk_id, reason) DO UPDATE SET
                permanent = excluded.permanent,
                expires_at = excluded.expires_at,
                created_at = excluded.created_at",
        )
        .bind(&s.chunk_id)
        .bind(&s.reason)
        .bind(s.permanent)
        .bind(&s.expires_at)
        .bind(&s.created_at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Chunk ids excluded from retrieval: any suppression that has not
    /// expired as of `now` (permanent rows never do).
    pub async fn list_suppressed_chunk_ids(&self, now: &str) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT chunk_id FROM suppressions
             WHERE permanent = 1 OR expires_at IS NULL OR expires_at > ?
             ORDER BY chunk_id",
        )
        .bind(now)
        .fetch_all(self.pool())
        .await?)
    }
}
