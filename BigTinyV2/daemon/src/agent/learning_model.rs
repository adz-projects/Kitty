//! Which model an app's memory learning runs on: adaptive-pathway's
//! extraction and cross-session synthesis, and memorabilia's extraction.
//!
//! Per app (`/api/apps/me/learning-model`), read per call so a change applies
//! to the next pass. No row means [`LearningModel::Auto`].

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LearningModel {
    /// The local helper model, else the chat's own provider (the app's
    /// provider for a pass with no chat). The engine's long-standing default.
    #[default]
    Auto,
    /// The chat's own provider first (the app's for a pass with no chat),
    /// keeping local CPU/GPU free; the helper model only if that fails.
    Chat,
    /// One provider row of the app's, optionally with a model override.
    /// Falls back to `Auto` when the call fails.
    Provider {
        provider_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
}

impl LearningModel {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            LearningModel::Provider { provider_id, .. } if provider_id.trim().is_empty() => {
                Err("provider_id must not be empty".into())
            }
            _ => Ok(()),
        }
    }
}

/// The app's choice; `Auto` when unset, unreadable or on any read error, so
/// learning never stops for a settings problem.
pub async fn get(pool: &SqlitePool, app_id: &str) -> LearningModel {
    let row: Option<String> =
        match sqlx::query_scalar("SELECT config FROM app_learning_model WHERE app_id = ?")
            .bind(app_id)
            .fetch_optional(pool)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("could not read the learning model for {app_id}: {e}");
                None
            }
        };
    row.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Whether the app has a row at all (for `GET`, which reports `null` then).
pub async fn get_stored(pool: &SqlitePool, app_id: &str) -> Result<Option<LearningModel>, String> {
    let row: Option<String> =
        sqlx::query_scalar("SELECT config FROM app_learning_model WHERE app_id = ?")
            .bind(app_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| e.to_string())?;
    Ok(row.and_then(|s| serde_json::from_str(&s).ok()))
}

/// Store `choice` for `app_id`; `None` clears it (back to `Auto`).
pub async fn set(pool: &SqlitePool, app_id: &str, choice: Option<&LearningModel>) -> Result<(), String> {
    match choice {
        Some(c) => {
            c.validate()?;
            let json = serde_json::to_string(c).map_err(|e| e.to_string())?;
            sqlx::query(
                "INSERT INTO app_learning_model (app_id, config, updated_at) \
                 VALUES (?, ?, datetime('now')) \
                 ON CONFLICT(app_id) DO UPDATE SET config = excluded.config, \
                 updated_at = excluded.updated_at",
            )
            .bind(app_id)
            .bind(json)
            .execute(pool)
            .await
            .map_err(|e| e.to_string())?;
        }
        None => {
            sqlx::query("DELETE FROM app_learning_model WHERE app_id = ?")
                .bind(app_id)
                .execute(pool)
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wire_shapes() {
        assert_eq!(serde_json::to_value(LearningModel::Auto).unwrap(), json!({"kind": "auto"}));
        assert_eq!(serde_json::to_value(LearningModel::Chat).unwrap(), json!({"kind": "chat"}));
        let p: LearningModel =
            serde_json::from_value(json!({"kind": "provider", "provider_id": "p1", "model": "m"}))
                .unwrap();
        assert_eq!(
            p,
            LearningModel::Provider { provider_id: "p1".into(), model: Some("m".into()) }
        );
        assert!(LearningModel::Provider { provider_id: " ".into(), model: None }
            .validate()
            .is_err());
    }

    #[tokio::test]
    async fn round_trip_and_default() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        assert_eq!(get(&pool, "a").await, LearningModel::Auto);
        set(&pool, "a", Some(&LearningModel::Chat)).await.unwrap();
        assert_eq!(get(&pool, "a").await, LearningModel::Chat);
        assert_eq!(get_stored(&pool, "b").await.unwrap(), None);
        set(&pool, "a", None).await.unwrap();
        assert_eq!(get(&pool, "a").await, LearningModel::Auto);
    }
}
