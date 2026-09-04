use chrono::DateTime;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};

use crate::error::StorageError;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct TimingRow {
    pub id: String,
    pub session_id: String,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub ttfb_ms: Option<f64>,
    pub ttft_ms: Option<f64>,
    pub generation_ms: Option<f64>,
    pub total_tokens: Option<i32>,
    pub created_at: Option<DateTime<Utc>>,
}

pub async fn insert_timing(pool: &SqlitePool, timing: &TimingRow) -> Result<(), StorageError> {
    sqlx::query(
        r#"INSERT INTO llm_timings (id, session_id, provider_id, model, ttfb_ms, ttft_ms, generation_ms, total_tokens)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?)"#
    )
    .bind(&timing.id)
    .bind(&timing.session_id)
    .bind(&timing.provider_id)
    .bind(&timing.model)
    .bind(timing.ttfb_ms)
    .bind(timing.ttft_ms)
    .bind(timing.generation_ms)
    .bind(timing.total_tokens)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_recent_timings(
    pool: &SqlitePool,
    session_id: &str,
    limit: i32,
) -> Result<Vec<TimingRow>, StorageError> {
    let rows = sqlx::query_as::<_, TimingRow>(
        r#"SELECT id, session_id, provider_id, model, ttfb_ms, ttft_ms, generation_ms, total_tokens, created_at
           FROM llm_timings WHERE session_id = ? ORDER BY created_at DESC, rowid DESC LIMIT ?"#
    )
    .bind(session_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
