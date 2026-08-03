use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::config::HITLConfig;
use crate::storage::hitl_rules;

/// Maximum age of a pending approval before it's swept stale.
const MAX_PENDING_AGE: Duration = Duration::from_secs(3600);

/// A pending tool call awaiting human approval.
#[derive(Debug, Clone)]
pub struct PendingAction {
    pub action_id: String,
    pub session_id: String,
    pub tool_name: String,
    pub tool_args: Value,
    /// Monotonic clock, used only for the stale-sweep age check.
    pub created_at: Instant,
    /// Wall-clock timestamp, used for the serialized `created_at` field —
    /// `Instant` has no fixed epoch, so what used to go out over the wire
    /// was "seconds since this action was created", not a real date. The
    /// UI (`GET /api/chat/{id}/pending`) renders that as a garbage date.
    pub created_at_utc: chrono::DateTime<Utc>,
}

impl PendingAction {
    pub fn to_dict(&self) -> Value {
        json!({
            "action_id": self.action_id,
            "session_id": self.session_id,
            "tool_name": self.tool_name,
            "tool_args": self.tool_args,
            "created_at": self.created_at_utc.to_rfc3339(),
        })
    }
}

/// Decision returned by HITL checks.
#[derive(Debug, Clone)]
pub struct HITLDecision {
    pub action: String,
    pub reason: Option<String>,
    pub pending_action_id: Option<String>,
}

impl HITLDecision {
    pub fn to_dict(&self) -> Value {
        json!({
            "action": self.action,
            "reason": self.reason,
            "pending_action_id": self.pending_action_id,
        })
    }
}

/// HITL (Human-In-The-Loop) manager: controls tool call approvals.
pub struct HITLManager {
    pool: SqlitePool,
    pub config: HITLConfig,
    pending: HashMap<String, PendingAction>,
    session_pending: HashMap<String, Vec<String>>,
    decisions: HashMap<String, (String, Instant)>,
}

impl HITLManager {
    pub fn new(pool: SqlitePool, config: HITLConfig) -> Self {
        Self {
            pool,
            config,
            pending: HashMap::new(),
            session_pending: HashMap::new(),
            decisions: HashMap::new(),
        }
    }

    /// Remove stale pending actions and decisions.
    fn sweep_stale(&mut self) {
        let cutoff = Instant::now() - MAX_PENDING_AGE;

        let stale_pending: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| p.created_at < cutoff)
            .map(|(k, _)| k.clone())
            .collect();

        for aid in stale_pending {
            if let Some(pending) = self.pending.remove(&aid) {
                if let Some(session_list) = self.session_pending.get_mut(&pending.session_id) {
                    session_list.retain(|id| id != &aid);
                    if session_list.is_empty() {
                        self.session_pending.remove(&pending.session_id);
                    }
                }
            }
        }

        let stale_decisions: Vec<String> = self
            .decisions
            .iter()
            .filter(|(_, (_, ts))| *ts < cutoff)
            .map(|(k, _)| k.clone())
            .collect();

        for aid in stale_decisions {
            self.decisions.remove(&aid);
        }
    }

    /// Check whether a tool call should proceed, be rejected, or need approval.
    pub async fn check_tool_call(
        &mut self,
        session_id: &str,
        tool_name: &str,
        args: &Value,
    ) -> HITLDecision {
        let args_str = serde_json::to_string(args).unwrap_or_default();

        // Check auto-reject patterns
        for pattern in &self.config.auto_reject_patterns {
            if args_str.contains(pattern.as_str()) || tool_name.contains(pattern.as_str()) {
                return HITLDecision {
                    action: "rejected".to_string(),
                    reason: Some(format!(
                        "Tool call matched auto-reject pattern: {}",
                        pattern
                    )),
                    pending_action_id: None,
                };
            }
        }

        // Check always-allow patterns
        for pattern in &self.config.always_allow_patterns {
            if tool_name.contains(pattern.as_str()) || args_str.contains(pattern.as_str()) {
                return HITLDecision {
                    action: "always_allow".to_string(),
                    reason: None,
                    pending_action_id: None,
                };
            }
        }

        // Check DB rules
        if let Ok(rules) = hitl_rules::list_rules_by_tool(&self.pool, tool_name).await {
            if let Some(rule) = Self::match_rule(&rules, &args_str) {
                return match rule.decision.as_str() {
                    "reject" => HITLDecision {
                        action: "rejected".to_string(),
                        reason: Some(format!(
                            "DB rule prevents this tool call: {}",
                            rule.args_pattern.as_deref().unwrap_or(tool_name)
                        )),
                        pending_action_id: None,
                    },
                    "allow" | "always_allow" => HITLDecision {
                        action: "proceed".to_string(),
                        reason: None,
                        pending_action_id: None,
                    },
                    _ => HITLDecision {
                        action: "proceed".to_string(),
                        reason: None,
                        pending_action_id: None,
                    },
                };
            }
        }

        // Apply default policy
        match self.config.default_policy.as_str() {
            "auto_allow" => HITLDecision {
                action: "proceed".to_string(),
                reason: None,
                pending_action_id: None,
            },
            "auto_reject" => HITLDecision {
                action: "rejected".to_string(),
                reason: Some(
                    "Default policy is auto-reject for unclassified tool calls".to_string(),
                ),
                pending_action_id: None,
            },
            _ => self.create_pending(session_id, tool_name, args, "requires human approval"),
        }
    }

    /// Force-escalate a tool call to need approval (used by sandbox).
    pub fn force_approval(
        &mut self,
        session_id: &str,
        tool_name: &str,
        args: &Value,
    ) -> HITLDecision {
        self.create_pending(
            session_id,
            tool_name,
            args,
            "wants to touch a path outside this session's allowed directories",
        )
    }

    fn create_pending(
        &mut self,
        session_id: &str,
        tool_name: &str,
        args: &Value,
        reason: &str,
    ) -> HITLDecision {
        self.sweep_stale();
        let action_id = uuid::Uuid::new_v4().to_string();
        let pending = PendingAction {
            action_id: action_id.clone(),
            session_id: session_id.to_string(),
            tool_name: tool_name.to_string(),
            tool_args: args.clone(),
            created_at: Instant::now(),
            created_at_utc: Utc::now(),
        };
        self.pending.insert(action_id.clone(), pending);
        self.session_pending
            .entry(session_id.to_string())
            .or_default()
            .push(action_id.clone());

        HITLDecision {
            action: "needs_approval".to_string(),
            reason: Some(format!("Tool '{}' {}", tool_name, reason)),
            pending_action_id: Some(action_id),
        }
    }

    /// Record a human decision for a pending action.
    pub async fn record_decision(&mut self, action_id: &str, decision: &str) -> HITLDecision {
        let pending = match self.pending.remove(action_id) {
            Some(p) => p,
            None => {
                return HITLDecision {
                    action: "rejected".to_string(),
                    reason: Some(format!("No pending action found: {}", action_id)),
                    pending_action_id: None,
                };
            }
        };

        if let Some(session_list) = self.session_pending.get_mut(&pending.session_id) {
            session_list.retain(|id| id != action_id);
            if session_list.is_empty() {
                self.session_pending.remove(&pending.session_id);
            }
        }

        self.decisions.insert(
            action_id.to_string(),
            (decision.to_string(), Instant::now()),
        );

        match decision {
            "reject" => HITLDecision {
                action: "rejected".to_string(),
                reason: Some(format!("User rejected tool call '{}'", pending.tool_name)),
                pending_action_id: None,
            },
            "always_allow" => {
                if let Err(e) =
                    hitl_rules::upsert_rule(&self.pool, &pending.tool_name, None, "always_allow")
                        .await
                {
                    tracing::error!("Failed to insert always_allow rule: {}", e);
                }
                HITLDecision {
                    action: "always_allow".to_string(),
                    reason: None,
                    pending_action_id: None,
                }
            }
            _ => HITLDecision {
                action: "proceed".to_string(),
                reason: None,
                pending_action_id: None,
            },
        }
    }

    /// Consume the resolved decision for an action.
    pub fn pop_decision(&mut self, action_id: &str) -> Option<String> {
        self.decisions
            .remove(action_id)
            .map(|(decision, _)| decision)
    }

    /// Get pending approvals for a session.
    pub fn get_pending_approvals(&self, session_id: &str) -> Vec<PendingAction> {
        self.session_pending
            .get(session_id)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.pending.get(id).cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Cancel all pending actions for a session.
    pub fn cancel_pending(&mut self, session_id: &str) {
        if let Some(action_ids) = self.session_pending.remove(session_id) {
            for aid in action_ids {
                self.pending.remove(&aid);
            }
        }
    }

    fn match_rule(
        rules: &[hitl_rules::HITLRuleRow],
        args_str: &str,
    ) -> Option<hitl_rules::HITLRuleRow> {
        for rule in rules {
            if let Some(pattern) = &rule.args_pattern {
                match regex::Regex::new(pattern) {
                    Ok(re) => {
                        if re.is_match(args_str) {
                            return Some(rule.clone());
                        }
                    }
                    Err(_) => {
                        if args_str.contains(pattern) {
                            return Some(rule.clone());
                        }
                    }
                }
            } else {
                return Some(rule.clone());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pending_action_to_dict() {
        let action = PendingAction {
            action_id: "test-1".to_string(),
            session_id: "sess-1".to_string(),
            tool_name: "read_file".to_string(),
            tool_args: json!({"path": "/test.txt"}),
            created_at: Instant::now(),
            created_at_utc: Utc::now(),
        };
        let dict = action.to_dict();
        assert_eq!(
            dict.get("action_id").and_then(|v| v.as_str()),
            Some("test-1")
        );
        assert_eq!(
            dict.get("tool_name").and_then(|v| v.as_str()),
            Some("read_file")
        );
    }

    #[test]
    fn test_hitl_decision_to_dict() {
        let decision = HITLDecision {
            action: "needs_approval".to_string(),
            reason: Some("Test reason".to_string()),
            pending_action_id: Some("action-1".to_string()),
        };
        let dict = decision.to_dict();
        assert_eq!(
            dict.get("action").and_then(|v| v.as_str()),
            Some("needs_approval")
        );
        assert_eq!(
            dict.get("reason").and_then(|v| v.as_str()),
            Some("Test reason")
        );
    }
}
