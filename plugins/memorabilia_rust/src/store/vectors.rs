//! `chunks_vec` backend for the `VectorIndex` seam (plan §3.3).
//!
//! The vec0 virtual table holds only `(chunk_id, embedding)`; the model tag
//! that produced a vector lives on the chunk row (`chunks.embedding_model`,
//! plan §5.1), so "filter by space" is a join onto the chunk catalog — the
//! same join enforces "cosine over **active** chunks only". A
//! `__lexical_hash__` row therefore never cosine-compares against a semantic
//! query, and vice versa (plan §3.3 regression guard).
//!
//! Search is a brute-force `vec_distance_cosine` scan: deterministic and
//! exact at single-user scale (plan §3.3 — correctness over approximate
//! recall). Ordering is distance, then `chunk_id`, so repeated calls agree
//! byte-for-byte (plan §13.4).
//!
//! Transactions: the two writes in `upsert` and every read here execute on
//! the single-connection pool; when a caller wraps them in
//! `Db::run_in_transaction` they are physically atomic with it (Phase 4
//! ingestion does exactly that), so the vector row and the chunk row's tag
//! can never diverge.

use sqlx::{Row, SqlitePool};

use crate::traits::VectorIndex;

use super::{decode_embedding, encode_embedding};
use async_trait::async_trait;

pub struct SqliteVectorIndex {
    pool: SqlitePool,
    /// The owning `Db`'s transaction lock, so a write here outside a
    /// transaction cannot land inside another task's (see `Db::write`).
    /// `None` for an index built straight from a pool (tests).
    txn_lock: Option<std::sync::Arc<tokio::sync::Mutex<()>>>,
    /// The owning `Db`'s read-only pool, for searches outside a transaction.
    reader: Option<SqlitePool>,
}

impl SqliteVectorIndex {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            txn_lock: None,
            reader: None,
        }
    }

    /// The index over `db`'s connection, sharing its write lock.
    pub fn for_db(db: &super::Db) -> Self {
        Self {
            pool: db.pool().clone(),
            txn_lock: Some(db.txn_lock.clone()),
            reader: Some(db.reader.clone()),
        }
    }

    /// Whether this task is inside the owning `Db`'s transaction.
    fn in_txn(&self) -> bool {
        self.txn_lock.as_ref().is_some_and(|lock| {
            let id = std::sync::Arc::as_ptr(lock) as usize;
            super::IN_TXN.try_with(|ids| ids.contains(&id)).unwrap_or(false)
        })
    }

    /// The read-only pool outside a transaction, else the write connection.
    fn reads(&self) -> &SqlitePool {
        match &self.reader {
            Some(r) if !self.in_txn() => r,
            _ => &self.pool,
        }
    }

    /// Hold the write lock for one write, unless this task is inside the
    /// owning `Db`'s transaction already.
    async fn write_lock(&self) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        if self.in_txn() {
            return None;
        }
        Some(self.txn_lock.as_ref()?.clone().lock_owned().await)
    }
}

#[async_trait]
impl VectorIndex for SqliteVectorIndex {
    async fn upsert(
        &self,
        chunk_id: &str,
        embedding: &[f32],
        model: &str,
    ) -> std::result::Result<(), String> {
        // Vector row first, then keep the chunk row's tag in lockstep
        // (plan §3.3: every vector is tagged with the model that produced
        // it). If the chunk row does not exist yet — ingestion embeds
        // before it writes the row — the tag update touches nothing and the
        // row carries the tag from its own insert with the same value.
        //
        // vec0 implements no UPSERT xUpdate, so "replace" is the
        // canonical delete-then-reinsert pair — both statements execute on
        // the single-connection pool and are atomic under the caller's
        // `run_in_transaction`.
        // One lock over the three statements: they are one replacement.
        let _write = self.write_lock().await;
        sqlx::query("DELETE FROM chunks_vec WHERE chunk_id = ?")
            .bind(chunk_id)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        sqlx::query("INSERT INTO chunks_vec (chunk_id, embedding) VALUES (?, ?)")
            .bind(chunk_id)
            .bind(encode_embedding(embedding))
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        sqlx::query("UPDATE chunks SET embedding_model = ? WHERE chunk_id = ?")
            .bind(model)
            .bind(chunk_id)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn remove(&self, chunk_id: &str) -> std::result::Result<(), String> {
        let _write = self.write_lock().await;
        sqlx::query("DELETE FROM chunks_vec WHERE chunk_id = ?")
            .bind(chunk_id)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn search(
        &self,
        query: &[f32],
        model: &str,
        k: usize,
    ) -> std::result::Result<Vec<(String, f32)>, String> {
        let rows = sqlx::query(
            "SELECT chunk_id, dist FROM (
                SELECT cv.chunk_id AS chunk_id,
                       vec_distance_cosine(cv.embedding, ?) AS dist
                FROM chunks_vec AS cv
                JOIN chunks AS c ON c.chunk_id = cv.chunk_id
                WHERE c.status = 'active' AND c.embedding_model = ?
             )
             WHERE dist IS NOT NULL
             ORDER BY dist ASC, chunk_id ASC
             LIMIT ?",
        )
        .bind(encode_embedding(query))
        .bind(model)
        .bind(k as i64)
        .fetch_all(self.reads())
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let id: String = r
                    .try_get("chunk_id")
                    .map_err(|e| e.to_string())?;
                // vec0 stores cosine *distance*; the seam speaks cosine.
                let dist: f64 = r
                    .try_get("dist")
                    .map_err(|e| e.to_string())?;
                Ok((id, (1.0 - dist) as f32))
            })
            .collect::<std::result::Result<Vec<_>, String>>()?)
    }

    async fn search_with_vectors(
        &self,
        query: &[f32],
        model: &str,
        k: usize,
    ) -> std::result::Result<Vec<(String, f32, Option<Vec<f32>>)>, String> {
        let rows = sqlx::query(
            "SELECT chunk_id, dist, embedding FROM (
                SELECT cv.chunk_id AS chunk_id,
                       vec_distance_cosine(cv.embedding, ?) AS dist,
                       cv.embedding AS embedding
                FROM chunks_vec AS cv
                JOIN chunks AS c ON c.chunk_id = cv.chunk_id
                WHERE c.status = 'active' AND c.embedding_model = ?
             )
             WHERE dist IS NOT NULL
             ORDER BY dist ASC, chunk_id ASC
             LIMIT ?",
        )
        .bind(encode_embedding(query))
        .bind(model)
        .bind(k as i64)
        .fetch_all(self.reads())
        .await
        .map_err(|e| e.to_string())?;
        rows.into_iter()
            .map(|r| {
                let id: String = r.try_get("chunk_id").map_err(|e| e.to_string())?;
                let dist: f64 = r.try_get("dist").map_err(|e| e.to_string())?;
                let bytes: Vec<u8> = r.try_get("embedding").map_err(|e| e.to_string())?;
                Ok((id, (1.0 - dist) as f32, Some(decode_embedding(&bytes))))
            })
            .collect()
    }

    async fn list_by_model(&self, model: &str) -> std::result::Result<Vec<String>, String> {
        let rows = sqlx::query(
            "SELECT cv.chunk_id AS chunk_id
             FROM chunks_vec AS cv
             JOIN chunks AS c ON c.chunk_id = cv.chunk_id
             WHERE c.embedding_model = ?
             ORDER BY cv.chunk_id",
        )
        .bind(model)
        .fetch_all(self.reads())
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|r| r.try_get("chunk_id").map_err(|e| e.to_string()))
            .collect::<std::result::Result<Vec<_>, String>>()?)
    }
}

/// Decode a stored vector back out (diagnostics/tests only — the search
/// path never materializes stored vectors; distances are computed in SQL).
pub async fn fetch_vector(pool: &SqlitePool, chunk_id: &str) -> crate::error::Result<Vec<f32>> {
    let bytes: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT embedding FROM chunks_vec WHERE chunk_id = ?")
            .bind(chunk_id)
        .fetch_optional(pool)
        .await?;
    Ok(bytes.as_deref().map(decode_embedding).unwrap_or_default())
}
