//! An optional **decision model**: a small classifier an app can point the
//! engine at, which answers typed questions (yes/no, pick one, rate) over a
//! piece of text in one forward pass.
//!
//! Everything that uses it is an optimisation of something the engine already
//! does, so every use **fails open**: no configuration, a disabled feature, an
//! unreachable endpoint or a slow answer all mean "do what you would have done
//! without it". Nothing here may turn a working turn into a failing one.
//!
//! The model is swappable. Callers ask [`Question`]s and read [`Answer`]s;
//! only a [`backend::DeciderBackend`] knows the wire format, chosen by the
//! app's `dialect`. Every cutoff a caller compares an answer against lives in
//! [`Thresholds`], configured per app, because a different model is calibrated
//! differently.
//!
//! Configuration is per app (`/api/apps/me/decider`): the engine is shared,
//! and one app's decision model must never steer another app's turns.

pub mod backend;
pub mod hub;
pub mod memory_gates;
pub mod systemone;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use hub::{DeciderHub, FeatureStats};

/// The dialects this build can speak. Adding a model that speaks something
/// else is one more [`backend::DeciderBackend`] and one more entry here.
pub const DIALECTS: &[&str] = &[systemone::DIALECT];

/// One question, in the Decision Index shape (`type` / `instructions` /
/// `criteria`) that every backend translates from.
#[derive(Debug, Clone, PartialEq)]
pub enum Question {
    /// Yes or no; the answer is P(yes).
    YesNo { instructions: String },
    /// One of named options, `(name, description)` in a stable order.
    Choice {
        instructions: String,
        options: Vec<(String, String)>,
    },
    /// An ordered rating, lowest level first (2 to 10 levels).
    Score {
        instructions: String,
        levels: Vec<String>,
    },
}

impl Question {
    pub fn yes_no(instructions: impl Into<String>) -> Self {
        Self::YesNo {
            instructions: instructions.into(),
        }
    }
}

/// One answer. Which fields are set depends on the question's type.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Answer {
    /// P(yes), for a [`Question::YesNo`].
    pub yes: Option<f32>,
    /// The chosen option, for a [`Question::Choice`].
    pub choice: Option<String>,
    /// For a [`Question::Score`]: the most likely level, 0-based.
    pub level: Option<usize>,
    /// The backend's confidence in `choice` / `level`, 0..=1.
    pub confidence: Option<f32>,
    /// Per-option (choice) or per-level (score, in level order) probabilities.
    pub probabilities: Vec<(String, f32)>,
}

impl Answer {
    /// P(yes), or `default` when the backend gave none.
    pub fn yes_or(&self, default: f32) -> f32 {
        self.yes.unwrap_or(default)
    }

    /// The probability of one named option.
    pub fn prob_of(&self, option: &str) -> Option<f32> {
        self.probabilities
            .iter()
            .find(|(k, _)| k == option)
            .map(|(_, p)| *p)
    }
}

/// Answers by question name.
pub type Answers = BTreeMap<String, Answer>;

/// The uses an app can switch on one by one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    ToolGating,
    LearnGate,
    RecallRerank,
    IngestGate,
    PageClassify,
    SearchRerank,
    Compaction,
}

impl Feature {
    pub const ALL: [Feature; 7] = [
        Feature::ToolGating,
        Feature::LearnGate,
        Feature::RecallRerank,
        Feature::IngestGate,
        Feature::PageClassify,
        Feature::SearchRerank,
        Feature::Compaction,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Feature::ToolGating => "tool_gating",
            Feature::LearnGate => "learn_gate",
            Feature::RecallRerank => "recall_rerank",
            Feature::IngestGate => "ingest_gate",
            Feature::PageClassify => "page_classify",
            Feature::SearchRerank => "search_rerank",
            Feature::Compaction => "compaction",
        }
    }
}

/// Which features are on. A missing key is off: an app that has not said it
/// wants a behaviour change does not get one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Features {
    pub tool_gating: bool,
    pub learn_gate: bool,
    pub recall_rerank: bool,
    pub ingest_gate: bool,
    pub page_classify: bool,
    pub search_rerank: bool,
    pub compaction: bool,
}

impl Features {
    pub fn is_on(&self, f: Feature) -> bool {
        match f {
            Feature::ToolGating => self.tool_gating,
            Feature::LearnGate => self.learn_gate,
            Feature::RecallRerank => self.recall_rerank,
            Feature::IngestGate => self.ingest_gate,
            Feature::PageClassify => self.page_classify,
            Feature::SearchRerank => self.search_rerank,
            Feature::Compaction => self.compaction,
        }
    }
}

/// Every cutoff an answer is compared against. The defaults are tuned for
/// Liquid's d1; an app overrides any of them (only the overrides need be
/// sent — missing fields take the default).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Thresholds {
    /// Tool gating, cold decision: load a group at P(needed) ≥ this. Low,
    /// because a missing tool costs more than an extra one. 0.20 and 0.80
    /// below were chosen by replaying 17 real exported chats (226 turns)
    /// against d1-3B: 3 missed turns and 2 additions, against 9 and 8 at
    /// 0.30 / 0.60.
    pub tool_gating_cold: f32,
    /// Tool gating, warm turn: add a group at P ≥ this. High, because each
    /// addition costs one prompt-cache miss, and at 0.60 six of eight
    /// additions in the replay were never used.
    pub tool_gating_add: f32,
    /// Skip a behavioural-memory learn pass when P(something durable) < this.
    pub learn_gate_skip_below: f32,
    /// Recall: drop a belief when P(relevant) < this (identity slots exempt).
    pub recall_drop_below: f32,
    /// Factual memory: reject a document when P(worth keeping) < this. On d1,
    /// a bot-check page and a navigation bar scored 0.21 and 0.16, real
    /// reference text 0.88 to 0.93.
    pub ingest_gate_reject_below: f32,
    /// Web: act on a page label only when its probability is ≥ this.
    pub page_classify_min: f32,
    /// Compaction: leave an exchange out of the summary at P(filler) ≥ this.
    pub compaction_drop_min: f32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            tool_gating_cold: 0.20,
            tool_gating_add: 0.80,
            learn_gate_skip_below: 0.15,
            recall_drop_below: 0.08,
            ingest_gate_reject_below: 0.35,
            page_classify_min: 0.70,
            compaction_drop_min: 0.65,
        }
    }
}

/// A named set of tools the tool gate loads or leaves out together. Supplied
/// by the app, which knows its own tools; the engine stays generic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolGroup {
    pub name: String,
    /// What the group is for, phrased as a need ("reading or writing Word,
    /// Excel or PDF files"). Becomes the question the model is asked.
    pub description: String,
    /// Tool names; a trailing `*` matches a prefix.
    pub tools: Vec<String>,
    /// Always offered, never gated.
    #[serde(default)]
    pub always: bool,
    /// File extensions (".docx", lowercase) that load this group outright
    /// when a file of that type is attached: an attachment says what tools a
    /// turn needs more reliably than its prose (measured on real chats, the
    /// decision model rated "documents" below 0.3 for a message attaching
    /// two .docx files).
    #[serde(default)]
    pub file_types: Vec<String>,
}

impl ToolGroup {
    /// Whether `file` (a path or name) has one of this group's file types.
    pub fn takes_file(&self, file: &str) -> bool {
        let lower = file.trim().to_lowercase();
        self.file_types
            .iter()
            .any(|ext| !ext.is_empty() && lower.ends_with(&ext.to_lowercase()))
    }

    pub fn matches(&self, tool: &str) -> bool {
        self.tools.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => tool.starts_with(prefix),
            None => tool == p,
        })
    }
}

/// An app's decision-model configuration, as stored and as PUT.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeciderConfig {
    #[serde(default = "default_dialect")]
    pub dialect: String,
    pub url: String,
    /// The model the app last saw at `url`, for display only.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub features: Features,
    #[serde(default)]
    pub thresholds: Thresholds,
    #[serde(default)]
    pub tool_groups: Vec<ToolGroup>,
    /// How long a provider keeps a prompt prefix cached, in seconds. A chat
    /// idle for longer has a cold cache, so the tool gate may re-decide (and
    /// shrink) its tool set for free. 300 matches Anthropic's and OpenAI's
    /// default cache lifetime; a llama-server slot can be evicted sooner.
    #[serde(default = "default_cache_ttl_secs")]
    pub cache_ttl_secs: u64,
}

fn default_dialect() -> String {
    systemone::DIALECT.to_string()
}

fn default_cache_ttl_secs() -> u64 {
    300
}

impl DeciderConfig {
    /// Reject what can never work, so a bad PUT fails loudly instead of
    /// silently disabling everything at turn time.
    pub fn validate(&self) -> Result<(), String> {
        if !DIALECTS.contains(&self.dialect.as_str()) {
            return Err(format!(
                "unknown dialect '{}' (supported: {})",
                self.dialect,
                DIALECTS.join(", ")
            ));
        }
        let url = reqwest::Url::parse(self.url.trim())
            .map_err(|e| format!("invalid url '{}': {e}", self.url))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("url must be http or https".into());
        }
        let mut seen = std::collections::HashSet::new();
        for g in &self.tool_groups {
            if g.name.trim().is_empty() || !seen.insert(g.name.as_str()) {
                return Err(format!(
                    "tool group names must be unique and non-empty ('{}')",
                    g.name
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_thresholds_take_the_defaults() {
        let c: DeciderConfig = serde_json::from_value(serde_json::json!({
            "url": "http://127.0.0.1:8080",
            "thresholds": {"tool_gating_add": 0.75},
            "features": {"tool_gating": true},
        }))
        .unwrap();
        assert_eq!(c.dialect, "systemone");
        assert_eq!(c.thresholds.tool_gating_add, 0.75);
        assert_eq!(c.thresholds.tool_gating_cold, 0.20);
        assert!(c.features.is_on(Feature::ToolGating));
        assert!(!c.features.is_on(Feature::Compaction));
        assert!(c.validate().is_ok());
    }

    #[test]
    fn validation_rejects_unknown_dialects_and_bad_urls() {
        let mut c: DeciderConfig =
            serde_json::from_value(serde_json::json!({"url": "http://x:1"})).unwrap();
        c.dialect = "nope".into();
        assert!(c.validate().is_err());
        c.dialect = "systemone".into();
        c.url = "ftp://x".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn group_patterns_match_names_and_prefixes() {
        let g = ToolGroup {
            name: "office".into(),
            description: String::new(),
            tools: vec!["lean_word_*".into(), "lean_pdf_read_text".into()],
            always: false,
            file_types: vec![],
        };
        assert!(g.matches("lean_word_read_text"));
        assert!(g.matches("lean_pdf_read_text"));
        assert!(!g.matches("lean_pdf_info"));
    }
}
