//! Per-app automatic approval: the calls an app would approve unattended
//! anyway, decided here so they never pause.
//!
//! The default HITL policy pauses on nearly every tool call, and Kitty used to
//! answer the safe ones itself: a round trip from the engine to Kitty and back
//! (a folder lookup, a metadata read, sometimes a decision-model call, then
//! the approval) before each tool could run. An app that sets a policy here
//! gets the same rule applied in place:
//!
//! - every path the call names lies inside the session's allowed folders
//!   (the same containment check the sandbox applies);
//! - a shell command does not match the app's own sensitive-command pattern;
//! - the decision model's shell screen, when the app has it on, does not flag
//!   the command.
//!
//! Only a call nothing else has decided about (no stored rule, no matching
//! pattern) is eligible, so a user's "reject" rule or a containment escalation
//! still stands. Anything not allowed here pauses exactly as before, and the
//! app's own approval handling sees it.

use std::sync::Arc;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::SqlitePool;

/// An app's policy document, as stored and as PUT.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalPolicyConfig {
    /// Approve eligible calls without pausing.
    #[serde(default)]
    pub auto_allow: bool,
    /// A regex (Rust `regex` syntax) matched against a shell command; a match
    /// means the command needs a person.
    #[serde(default)]
    pub shell_sensitive_pattern: Option<String>,
}

/// A loaded policy.
pub struct ApprovalPolicy {
    pub config: ApprovalPolicyConfig,
    sensitive: Option<regex::Regex>,
}

impl ApprovalPolicy {
    fn new(config: ApprovalPolicyConfig) -> Result<Self, String> {
        let sensitive = match config.shell_sensitive_pattern.as_deref() {
            Some(p) if !p.trim().is_empty() => {
                Some(regex::Regex::new(p).map_err(|e| format!("bad shell_sensitive_pattern: {e}"))?)
            }
            _ => None,
        };
        Ok(Self { config, sensitive })
    }

    /// The shell command a call carries, if any.
    fn command(args: &Value) -> Option<&str> {
        ["command", "cmd", "script"]
            .iter()
            .find_map(|k| args.get(*k).and_then(Value::as_str))
            .filter(|c| !c.trim().is_empty())
    }

    /// Whether the rule allows this call without the decision model's view:
    /// `Some(command)` when a shell command still needs the screen.
    pub fn allows<'a>(&self, args: &'a Value, allowed_dirs: &[String]) -> Option<Option<&'a str>> {
        if !self.config.auto_allow {
            return None;
        }
        if !crate::agent::sandbox::check_containment(args, allowed_dirs, false) {
            return None;
        }
        match Self::command(args) {
            Some(cmd) if self.sensitive.as_ref().is_some_and(|re| re.is_match(cmd)) => None,
            Some(cmd) => Some(Some(cmd)),
            None => Some(None),
        }
    }
}

/// Every app's policy, cached.
pub struct ApprovalPolicies {
    pool: SqlitePool,
    cache: DashMap<String, Option<Arc<ApprovalPolicy>>>,
}

impl ApprovalPolicies {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            cache: DashMap::new(),
        }
    }

    /// The app's policy, if it set one.
    pub async fn for_app(&self, app_id: &str) -> Option<Arc<ApprovalPolicy>> {
        if let Some(hit) = self.cache.get(app_id) {
            return hit.clone();
        }
        let raw: Option<String> =
            match sqlx::query_scalar("SELECT config FROM app_approval_policy WHERE app_id = ?")
                .bind(app_id)
                .fetch_optional(&self.pool)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(app = app_id, "could not read the approval policy: {e}");
                    return None;
                }
            };
        let loaded = raw
            .and_then(|r| serde_json::from_str::<ApprovalPolicyConfig>(&r).ok())
            .and_then(|c| ApprovalPolicy::new(c).ok())
            .map(Arc::new);
        self.cache.insert(app_id.to_string(), loaded.clone());
        loaded
    }

    /// Replace (or with `None`, remove) an app's policy.
    pub async fn set(&self, app_id: &str, config: Option<ApprovalPolicyConfig>) -> Result<(), String> {
        match &config {
            Some(c) => {
                let policy = ApprovalPolicy::new(c.clone())?;
                let raw = serde_json::to_string(c).map_err(|e| e.to_string())?;
                sqlx::query(
                    "INSERT INTO app_approval_policy (app_id, config, updated_at) \
                     VALUES (?, ?, datetime('now')) \
                     ON CONFLICT(app_id) DO UPDATE SET config = excluded.config, \
                     updated_at = excluded.updated_at",
                )
                .bind(app_id)
                .bind(raw)
                .execute(&self.pool)
                .await
                .map_err(|e| e.to_string())?;
                self.cache.insert(app_id.to_string(), Some(Arc::new(policy)));
            }
            None => {
                sqlx::query("DELETE FROM app_approval_policy WHERE app_id = ?")
                    .bind(app_id)
                    .execute(&self.pool)
                    .await
                    .map_err(|e| e.to_string())?;
                self.cache.insert(app_id.to_string(), None);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy() -> ApprovalPolicy {
        ApprovalPolicy::new(ApprovalPolicyConfig {
            auto_allow: true,
            shell_sensitive_pattern: Some(r"(?i)\b(ssh|del)\b".into()),
        })
        .unwrap()
    }

    #[test]
    fn a_path_inside_the_folders_is_allowed_and_one_outside_is_not() {
        let dirs = vec!["C:/chats/c1".to_string()];
        let p = policy();
        assert_eq!(p.allows(&json!({"path": "C:/chats/c1/a.md"}), &dirs), Some(None));
        assert_eq!(p.allows(&json!({"path": "C:/Windows/x"}), &dirs), None);
    }

    #[test]
    fn a_sensitive_command_needs_a_person_and_others_go_to_the_screen() {
        let p = policy();
        assert_eq!(p.allows(&json!({"command": "ssh box"}), &[]), None);
        assert_eq!(
            p.allows(&json!({"command": "git status"}), &[]),
            Some(Some("git status"))
        );
    }

    #[test]
    fn auto_allow_off_allows_nothing() {
        let p = ApprovalPolicy::new(ApprovalPolicyConfig {
            auto_allow: false,
            shell_sensitive_pattern: None,
        })
        .unwrap();
        assert_eq!(p.allows(&json!({"path": "a"}), &[]), None);
    }

    #[tokio::test]
    async fn set_persists_and_a_bad_pattern_is_refused() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let h = ApprovalPolicies::new(pool);
        assert!(h.for_app("kitty").await.is_none());
        let bad = ApprovalPolicyConfig {
            auto_allow: true,
            shell_sensitive_pattern: Some("(".into()),
        };
        assert!(h.set("kitty", Some(bad)).await.is_err());
        h.set("kitty", Some(policy().config)).await.unwrap();
        h.cache.clear();
        assert!(h.for_app("kitty").await.unwrap().config.auto_allow);
        h.set("kitty", None).await.unwrap();
        h.cache.clear();
        assert!(h.for_app("kitty").await.is_none());
    }
}
