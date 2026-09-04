use chrono::DateTime;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobConfig {
    pub name: String,
    pub cron: String,
    pub recipe_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleJob {
    pub id: String,
    pub name: String,
    pub cron: String,
    pub recipe_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

impl ScheduleJob {
    pub fn new(
        name: impl Into<String>,
        cron: impl Into<String>,
        recipe_id: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            cron: cron.into(),
            recipe_id: recipe_id.into(),
            parameters: None,
            enabled: true,
            created_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeParameter {
    pub name: String,
    #[serde(rename = "type")]
    pub param_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recipe {
    pub id: String,
    pub name: String,
    pub prompt_template: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Vec<RecipeParameter>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_mcp_servers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt_layer: Option<String>,
    pub max_steps: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

impl Recipe {
    pub fn new(name: impl Into<String>, prompt_template: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            prompt_template: prompt_template.into(),
            instructions: None,
            parameters: None,
            required_mcp_servers: None,
            system_prompt_layer: None,
            max_steps: 30,
            created_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schedule_job_new() {
        let job = ScheduleJob::new("daily", "0 0 * * *", "recipe1");
        assert!(job.enabled);
        assert_eq!(job.cron, "0 0 * * *");
    }

    #[test]
    fn test_recipe_new() {
        let r = Recipe::new("greet", "Hello {{name}}");
        assert_eq!(r.max_steps, 30);
        assert_eq!(r.name, "greet");
    }
}
