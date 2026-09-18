//! Stage 1 document rows + SQL primitives (plan §3.1): the whole-payload
//! SHA-256 catalog consulted for abort-on-seen before any chunk work.
//! Row types and parameterized SQL only.

use sqlx::FromRow;

use crate::error::Result;
use crate::store::Db;

#[derive(Debug, Clone, FromRow)]
pub struct Document {
    pub document_hash: String,
    pub source_entity: String,
    pub source_type: String,
    pub source_name: String,
    pub captured_at: String,
    /// Attachment intent (evidence vs casual, plan §4.2): the seed
    /// multiplier for this payload's `source_reliability`.
    pub intent_factor: f64,
    pub chunk_count: i64,
    pub created_at: String,
}

impl Db {
    /// Idempotent insert (abort-on-seen re-runs a document when an earlier
    /// run failed mid-way; the primary key makes the replay a no-op). The
    /// conflict target is explicit: a bad tier/CHECK/FK value must still
    /// surface as an error, only the dedup conflict is absorbed.
    pub async fn insert_document(&self, d: &Document) -> Result<()> {
        sqlx::query(
            "INSERT INTO documents (
                document_hash, source_entity, source_type, source_name,
                captured_at, intent_factor, chunk_count, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (document_hash) DO NOTHING",
        )
        .bind(&d.document_hash)
        .bind(&d.source_entity)
        .bind(&d.source_type)
        .bind(&d.source_name)
        .bind(&d.captured_at)
        .bind(d.intent_factor)
        .bind(d.chunk_count)
        .bind(&d.created_at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn get_document(&self, document_hash: &str) -> Result<Option<Document>> {
        Ok(sqlx::query_as::<_, Document>(
            "SELECT * FROM documents WHERE document_hash = ?",
        )
        .bind(document_hash)
        .fetch_optional(self.pool())
        .await?)
    }

    /// Set the document's final chunk count. Stage 3 writes the document
    /// row (the FK parent of its chunks) at the start of the document's
    /// transaction and seals it with this count before commit.
    pub async fn set_document_chunk_count(
        &self,
        document_hash: &str,
        chunk_count: i64,
    ) -> Result<()> {
        sqlx::query("UPDATE documents SET chunk_count = ? WHERE document_hash = ?")
            .bind(chunk_count)
            .bind(document_hash)
            .execute(self.pool())
            .await?;
        Ok(())
    }
}
