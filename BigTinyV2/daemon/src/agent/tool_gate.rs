//! Per-chat tool sets that keep the prompt cache warm.
//!
//! An app with a decision model and `tool_gating` on supplies a catalog of
//! tool groups (`decider::ToolGroup`). Each chat is offered only the groups it
//! needs — fewer schemas is what lets a small model pick the right tool — but
//! the tool list sits at the very front of the prompt, so changing it costs a
//! full prompt-cache miss. Hence the rules (see `prompt_determinism.md`):
//!
//! 1. **Decide once, then only grow.** A new chat's set is decided from its
//!    first message. Later turns ask only about groups not yet loaded, at a
//!    higher bar, and never remove one. Each addition is one miss.
//! 2. **Shrink only when the cache is already cold**: the chat has been
//!    compacted since the set was decided (the prompt head was rewritten
//!    anyway), it has been idle past the cache lifetime, or it moved to
//!    another provider.
//!
//! When the decision model can't answer, nothing changes: a chat with a set
//! keeps it, and a chat without one gets every tool and records nothing — so
//! the first real decision for a chat already under way waits for a cold
//! moment instead of shrinking a warm prefix.
//!
//! This module is the policy, pure; the loop does the I/O.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::decider::{Answers, Question, ToolGroup};
use crate::models::mcp::ToolDefinition;

/// Session-metadata key holding a chat's [`GateState`].
pub const META_KEY: &str = "tool_gate";

/// The loop-only tool a model calls to load a group it was not offered.
pub const LOAD_GROUP_TOOL: &str = "load_tool_group";

/// What the gate decided for a chat, kept in its metadata.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GateState {
    /// Loaded groups, sorted.
    pub groups: BTreeSet<String>,
    /// `compacted_through_rowid` when the set was last decided from scratch.
    pub compacted: i64,
    /// The provider the set was decided on.
    pub provider: String,
    /// Bumped whenever the set is re-decided (and so may shrink).
    pub epoch: u32,
}

impl GateState {
    pub fn read(metadata: &Value) -> Option<Self> {
        serde_json::from_value(metadata.get(META_KEY)?.clone()).ok()
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Why a decision runs from scratch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColdReason {
    NewChat,
    Compacted,
    Idle,
    ProviderChanged,
}

/// What this turn may do with the tool set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moment {
    /// Decide the whole set; it may shrink.
    Cold(ColdReason),
    /// Only add groups.
    Warm,
    /// A chat already under way with no recorded set (the decision model was
    /// unavailable when it started): offer everything, ask nothing, until a
    /// cold moment comes.
    Waiting,
}

/// The facts about a chat this turn that decide its [`Moment`].
#[derive(Debug, Clone)]
pub struct Situation<'a> {
    pub state: Option<&'a GateState>,
    /// Whether the chat has any messages before this turn's.
    pub has_history: bool,
    pub compacted_now: i64,
    pub provider_now: &'a str,
    /// Time since the chat's last stored message, if known.
    pub idle: Option<Duration>,
    pub cache_ttl: Duration,
}

pub fn moment(s: &Situation<'_>) -> Moment {
    let idle_past_ttl = s.idle.is_some_and(|i| i >= s.cache_ttl);
    match s.state {
        None if !s.has_history => Moment::Cold(ColdReason::NewChat),
        None if idle_past_ttl => Moment::Cold(ColdReason::Idle),
        None => Moment::Waiting,
        Some(st) if st.provider != s.provider_now => Moment::Cold(ColdReason::ProviderChanged),
        Some(st) if s.compacted_now > st.compacted => Moment::Cold(ColdReason::Compacted),
        Some(_) if idle_past_ttl => Moment::Cold(ColdReason::Idle),
        Some(_) => Moment::Warm,
    }
}

/// The groups the gate may leave out (not `always`).
pub fn gateable(catalog: &[ToolGroup]) -> Vec<&ToolGroup> {
    catalog.iter().filter(|g| !g.always).collect()
}

/// The gated group a tool belongs to, if any. A tool also matched by an
/// `always` group is never gated.
pub fn group_of<'a>(catalog: &'a [ToolGroup], tool: &str) -> Option<&'a ToolGroup> {
    if catalog.iter().any(|g| g.always && g.matches(tool)) {
        return None;
    }
    catalog.iter().find(|g| !g.always && g.matches(tool))
}

/// One yes/no question per group, named by the group.
pub fn questions<'a>(groups: impl IntoIterator<Item = &'a ToolGroup>) -> Vec<(String, Question)> {
    groups
        .into_iter()
        .map(|g| {
            (
                g.name.clone(),
                Question::yes_no(format!(
                    "Will answering this request need tools for {}?",
                    g.description
                )),
            )
        })
        .collect()
}

/// A from-scratch decision: every gateable group the model rates at or above
/// `threshold`. A group it gave no answer for is loaded — a missing tool costs
/// more than an extra one.
pub fn decide_cold(catalog: &[ToolGroup], answers: &Answers, threshold: f32) -> BTreeSet<String> {
    gateable(catalog)
        .into_iter()
        .filter(|g| {
            answers
                .get(&g.name)
                .and_then(|a| a.yes)
                .is_none_or(|p| p >= threshold)
        })
        .map(|g| g.name.clone())
        .collect()
}

/// A warm turn: the current set plus any unloaded group rated at or above
/// `threshold`. Never removes. Here a missing answer adds nothing, because
/// adding costs a cache miss and the safety valve covers a wrong "no".
pub fn decide_warm(
    current: &BTreeSet<String>,
    answers: &Answers,
    threshold: f32,
) -> BTreeSet<String> {
    let mut next = current.clone();
    for (name, a) in answers {
        if a.yes.is_some_and(|p| p >= threshold) {
            next.insert(name.clone());
        }
    }
    next
}

/// Keep the tools that are ungated or in a loaded group. Order is preserved
/// (the registry's sorted order), so the result is a stable prefix for any
/// given set. Returns how many were withheld.
pub fn filter(
    tools: &mut Vec<ToolDefinition>,
    catalog: &[ToolGroup],
    loaded: &BTreeSet<String>,
) -> usize {
    let before = tools.len();
    tools.retain(|t| group_of(catalog, &t.name).is_none_or(|g| loaded.contains(&g.name)));
    before - tools.len()
}

/// The safety valve's definition. Its description lists **every** gateable
/// group, loaded or not, so it never changes as groups load — a description
/// that tracked the set would itself break the cache.
pub fn load_group_tool(catalog: &[ToolGroup]) -> ToolDefinition {
    let groups: Vec<String> = gateable(catalog)
        .iter()
        .map(|g| format!("`{}` ({})", g.name, g.description))
        .collect();
    let names: Vec<&str> = gateable(catalog).iter().map(|g| g.name.as_str()).collect();
    ToolDefinition {
        name: LOAD_GROUP_TOOL.to_string(),
        description: format!(
            "Load a group of tools you were not given, when this request needs them. \
             Groups: {}. Groups already loaded are a no-op.",
            groups.join("; ")
        ),
        input_schema: json!({
            "type": "object",
            "properties": {"group": {"type": "string", "enum": names}},
            "required": ["group"],
        }),
        server_id: String::new(),
    }
}

/// The state text for a cold decision mid-chat: the last few exchanges, each
/// message trimmed, then the new message. Recent context is what a tool set
/// should fit, not the whole history.
pub fn recent_state(recent: &[(String, String)], user_message: &str) -> String {
    const PER_MESSAGE: usize = 500;
    let mut out = String::new();
    for (role, content) in recent {
        let c: String = content.chars().take(PER_MESSAGE).collect();
        out.push_str(&format!("{role}: {c}\n"));
    }
    out.push_str(&format!("user: {user_message}"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decider::Answer;

    fn catalog() -> Vec<ToolGroup> {
        let g = |name: &str, tools: &[&str], always: bool| ToolGroup {
            name: name.into(),
            description: format!("{name} work"),
            tools: tools.iter().map(|s| s.to_string()).collect(),
            always,
        };
        vec![
            g("core", &["lean_file_*"], true),
            g("office", &["lean_word_*", "lean_pdf_*"], false),
            g("web", &["lean_web_*"], false),
            g("viz", &["generate_*"], false),
        ]
    }

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: String::new(),
            input_schema: json!({}),
            server_id: String::new(),
        }
    }

    fn answers(ps: &[(&str, f32)]) -> Answers {
        ps.iter()
            .map(|(n, p)| {
                (
                    n.to_string(),
                    Answer {
                        yes: Some(*p),
                        ..Answer::default()
                    },
                )
            })
            .collect()
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn situation<'a>(state: Option<&'a GateState>, history: bool) -> Situation<'a> {
        Situation {
            state,
            has_history: history,
            compacted_now: 0,
            provider_now: "p1",
            idle: Some(Duration::from_secs(10)),
            cache_ttl: Duration::from_secs(300),
        }
    }

    #[test]
    fn a_new_chat_decides_from_scratch() {
        assert_eq!(
            moment(&situation(None, false)),
            Moment::Cold(ColdReason::NewChat)
        );
        let picked = decide_cold(
            &catalog(),
            &answers(&[("office", 0.9), ("web", 0.1), ("viz", 0.35)]),
            0.3,
        );
        assert_eq!(picked, set(&["office", "viz"]));
    }

    #[test]
    fn a_cold_decision_loads_a_group_it_got_no_answer_for() {
        let picked = decide_cold(&catalog(), &answers(&[("office", 0.05)]), 0.3);
        assert_eq!(picked, set(&["viz", "web"]));
    }

    #[test]
    fn a_warm_turn_only_ever_adds() {
        let st = GateState {
            groups: set(&["office"]),
            provider: "p1".into(),
            ..GateState::default()
        };
        assert_eq!(moment(&situation(Some(&st), true)), Moment::Warm);
        let next = decide_warm(&st.groups, &answers(&[("web", 0.7), ("viz", 0.4)]), 0.6);
        assert_eq!(next, set(&["office", "web"]));
        // A low score for a loaded group never removes it.
        let next = decide_warm(&st.groups, &answers(&[("office", 0.0)]), 0.6);
        assert_eq!(next, set(&["office"]));
    }

    #[test]
    fn cold_moments_are_compaction_idleness_and_a_provider_change() {
        let st = GateState {
            groups: set(&["office"]),
            compacted: 10,
            provider: "p1".into(),
            epoch: 0,
        };
        let mut s = situation(Some(&st), true);
        s.compacted_now = 42;
        assert_eq!(moment(&s), Moment::Cold(ColdReason::Compacted));
        let mut s = situation(Some(&st), true);
        s.idle = Some(Duration::from_secs(301));
        assert_eq!(moment(&s), Moment::Cold(ColdReason::Idle));
        let mut s = situation(Some(&st), true);
        s.provider_now = "p2";
        assert_eq!(moment(&s), Moment::Cold(ColdReason::ProviderChanged));
    }

    #[test]
    fn a_chat_under_way_without_a_set_waits_for_a_cold_moment() {
        assert_eq!(moment(&situation(None, true)), Moment::Waiting);
        let mut s = situation(None, true);
        s.idle = Some(Duration::from_secs(900));
        assert_eq!(moment(&s), Moment::Cold(ColdReason::Idle));
    }

    #[test]
    fn filtering_keeps_ungated_and_loaded_tools_in_order() {
        let mut tools = vec![
            tool("generate_chart"),
            tool("lean_file_read"),
            tool("lean_pdf_read_text"),
            tool("lean_web_search"),
            tool("record"),
        ];
        let withheld = filter(&mut tools, &catalog(), &set(&["office"]));
        assert_eq!(withheld, 2);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["lean_file_read", "lean_pdf_read_text", "record"]);
    }

    #[test]
    fn the_safety_valve_description_does_not_depend_on_what_is_loaded() {
        let a = load_group_tool(&catalog());
        assert!(a.description.contains("`web`"));
        assert!(!a.description.contains("`core`"));
        assert_eq!(
            a.input_schema["properties"]["group"]["enum"],
            json!(["office", "web", "viz"])
        );
    }

    #[test]
    fn state_round_trips_through_metadata() {
        let st = GateState {
            groups: set(&["web"]),
            compacted: 3,
            provider: "p".into(),
            epoch: 2,
        };
        let meta = json!({ META_KEY: st.to_value() });
        assert_eq!(GateState::read(&meta), Some(st));
        assert_eq!(GateState::read(&json!({})), None);
    }
}
