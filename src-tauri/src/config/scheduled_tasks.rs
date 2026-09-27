//! Scheduled tasks as an older Kitty kept them, in its own config, and the
//! schedule shape the settings form still speaks.
//!
//! The engine runs scheduled tasks now (`commands::scheduled_tasks`); these
//! types remain for moving tasks saved before that across, once, and for the
//! form's one-shot/recurring choice.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

/// `OneShot` fires once then disables itself. `Recurring` re-fires every
/// `interval_secs`; a run missed while the engine was down is caught up once
/// when it starts, not once per missed interval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    OneShot,
    Recurring { interval_secs: u64 },
}

/// A scheduled instruction as an older Kitty stored it. Firing meant a
/// brand-new session in `cwd` with `prompt` as its first message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub id: String,
    pub name: String,
    pub prompt: String,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Model this task runs on, overriding whatever provider is active when
    /// it fires (D3). `None` = use the active provider, which is what every
    /// task did before this existed.
    ///
    /// A *model* id, not a provider id: the task is stamped onto the session
    /// it creates via `set_session_provider`, which pairs the model with the
    /// provider resolved at fire time. Storing a provider id instead would go
    /// stale the moment a profile is deleted and recreated.
    #[serde(default)]
    pub model_id: Option<String>,
    pub schedule: Schedule,
    /// When this task should next fire. Advanced after each run: `now +
    /// interval_secs` for `Recurring`; for `OneShot`, `enabled` flips to
    /// `false` instead (kept in the list, greyed out, rather than deleted —
    /// deleting is a separate explicit user action).
    pub next_fire: DateTime<Local>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_shot_round_trips_through_json() {
        let t = ScheduledTask {
            id: "task_1".to_string(),
            name: "Morning summary".to_string(),
            prompt: "Summarize my open PRs".to_string(),
            cwd: None,
            model_id: None,
            schedule: Schedule::OneShot,
            next_fire: Local::now(),
            enabled: true,
        };
        let text = serde_json::to_string(&t).unwrap();
        let back: ScheduledTask = serde_json::from_str(&text).unwrap();
        assert_eq!(back.id, "task_1");
        assert!(matches!(back.schedule, Schedule::OneShot));
    }

    #[test]
    fn recurring_round_trips_with_interval() {
        let t = ScheduledTask {
            id: "task_2".to_string(),
            name: "Hourly check".to_string(),
            prompt: "Check for new alerts".to_string(),
            cwd: Some("C:/Users/me/chats/task_2".to_string()),
            model_id: Some("LFM2.5-1.2B-Instruct-Q4_K_M".to_string()),
            schedule: Schedule::Recurring {
                interval_secs: 3600,
            },
            next_fire: Local::now(),
            enabled: true,
        };
        let text = serde_json::to_string(&t).unwrap();
        let back: ScheduledTask = serde_json::from_str(&text).unwrap();
        match back.schedule {
            Schedule::Recurring { interval_secs } => assert_eq!(interval_secs, 3600),
            Schedule::OneShot => panic!("expected Recurring"),
        }
        assert_eq!(back.model_id.as_deref(), Some("LFM2.5-1.2B-Instruct-Q4_K_M"));
    }

    /// A task saved before `model_id` existed must still load, meaning "use
    /// whatever provider is active" — the behaviour it had when it was
    /// written. `#[serde(default)]` covers it; this pins that it does.
    #[test]
    fn a_task_predating_model_overrides_still_loads() {
        let json = r#"{
            "id": "old", "name": "Legacy", "prompt": "do the thing",
            "schedule": {"kind": "one_shot"},
            "next_fire": "2026-01-01T00:00:00+00:00", "enabled": true
        }"#;
        let back: ScheduledTask = serde_json::from_str(json).unwrap();
        assert_eq!(back.model_id, None);
        assert_eq!(back.cwd, None);
    }
}
