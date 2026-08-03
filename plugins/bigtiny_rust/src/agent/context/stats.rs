use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::storage::messages;
use crate::storage::sessions;

/// Session statistics: token counts, usage history, compaction state.
pub struct SessionStats {
    pool: SqlitePool,
}

impl SessionStats {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Get statistics for a session.
    pub async fn get_stats(&self, session_id: &str) -> Result<serde_json::Value, String> {
        let messages_list = messages::get_messages_by_session(&self.pool, session_id)
            .await
            .map_err(|e| format!("Failed to fetch messages: {}", e))?;

        let session = sessions::get_session(&self.pool, session_id)
            .await
            .map_err(|e| format!("Failed to fetch session: {}", e))?
            .ok_or_else(|| format!("Session {} not found", session_id))?;

        let tokens_sent: i32 = messages_list
            .iter()
            .filter(|m| m.role == "user" || m.role == "system")
            .map(|m| m.token_count.unwrap_or(0))
            .sum();

        let tokens_received: i32 = messages_list
            .iter()
            .filter(|m| m.role == "assistant")
            .map(|m| m.token_count.unwrap_or(0))
            .sum();

        let current_context: i32 = messages_list
            .iter()
            .map(|m| m.token_count.unwrap_or(0))
            .sum();

        let meta: serde_json::Value = session
            .metadata
            .as_ref()
            .and_then(|m| serde_json::from_str(m).ok())
            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

        let cost_tokens = tokens_sent + tokens_received;
        let estimated_cost = if cost_tokens > 0 {
            round_6(cost_tokens as f64 * 0.000003)
        } else {
            0.0
        };

        let memory_slots: Option<Value> = session
            .memory_slots
            .as_ref()
            .and_then(|s| serde_json::from_str(s).ok());

        Ok(json!({
            "session_id": session_id,
            "message_count": messages_list.len(),
            "tokens_sent": tokens_sent,
            "tokens_received": tokens_received,
            "current_context_tokens": current_context,
            "estimated_cost_usd": estimated_cost,
            "provider_history": meta.get("usage").unwrap_or(&json!([])),
            "compacted_through_rowid": session.compacted_through_rowid,
            "memory_slots": memory_slots,
        }))
    }

    /// Record LLM usage for a session.
    pub async fn record_usage(
        &self,
        session_id: &str,
        prompt_tokens: i32,
        completion_tokens: i32,
        provider: &str,
        model: &str,
    ) -> Result<(), String> {
        sessions::update_metadata_with(&self.pool, session_id, move |mut meta| {
            let mut usage: Vec<serde_json::Value> = meta
                .get("usage")
                .and_then(|u| u.as_array())
                .cloned()
                .unwrap_or_default();

            usage.push(json!({
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "provider": provider,
                "model": model,
            }));

            if usage.len() > 100 {
                usage = usage[usage.len() - 100..].to_vec();
            }

            if let Some(obj) = meta.as_object_mut() {
                obj.insert("usage".to_string(), json!(usage));
            }
            meta
        })
        .await
        .map_err(|e| format!("Failed to update session config: {}", e))
    }
}

fn round_6(v: f64) -> f64 {
    (v * 1_000_000.0).round() / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_6() {
        assert!((round_6(0.1234567) - 0.123457).abs() < f64::EPSILON);
        assert_eq!(round_6(0.0), 0.0);
    }
}
