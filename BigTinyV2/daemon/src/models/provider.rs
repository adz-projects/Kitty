use chrono::DateTime;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ProviderType {
    #[default]
    OpenaiCompat,
    Anthropic,
}

/// Canonical provider metadata shared between provider module and storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_length: Option<i32>,
}

/// Canonical health status shared between provider module and routes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthStatus {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub name: String,
    pub provider_type: ProviderType,
    pub base_url: String,
    pub fallback_priority: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

impl ProviderConfig {
    pub fn new(
        name: impl Into<String>,
        provider_type: ProviderType,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            provider_type,
            base_url: base_url.into(),
            fallback_priority: 1,
            config: None,
            status: "disconnected".into(),
            error_message: None,
            created_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_config_new() {
        let cfg = ProviderConfig::new("test", ProviderType::Anthropic, "https://api.anthropic.com");
        assert_eq!(cfg.name, "test");
        assert_eq!(cfg.provider_type, ProviderType::Anthropic);
        assert_eq!(cfg.status, "disconnected");
    }

    #[test]
    fn test_provider_type_serde() {
        let parsed: ProviderType = serde_json::from_str("\"anthropic\"").unwrap();
        assert_eq!(parsed, ProviderType::Anthropic);
        let json = serde_json::to_string(&ProviderType::OpenaiCompat).unwrap();
        assert_eq!(json, "\"openai_compat\"");
    }
}
