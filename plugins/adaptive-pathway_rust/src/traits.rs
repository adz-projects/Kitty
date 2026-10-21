//! Trait that inverts the AP→daemon dependency. `adaptive_pathway` defines a
//! `StructuredChat` abstraction for "give me a JSON-schema-constrained chat
//! completion"; the daemon implements it over its `SummarizerChain` (local
//! llama.cpp first, then whatever chat provider is actually configured — see
//! `bigtiny_rust::agent::summarizer_chain`). This lets the engine depend on a
//! plain interface instead of a concrete daemon type, resolving the circular
//! dependency (orphan rule permits: foreign trait + local type inside
//! bigtiny_rust).

use async_trait::async_trait;
use serde_json::Value;

#[async_trait]
pub trait StructuredChat: Send + Sync {
    /// Request a structured (JSON-schema-constrained) completion. Returns the
    /// parsed JSON content, or an error for the caller to treat as "this
    /// learn/consolidate pass is skipped" (never a hard failure).
    async fn structured_chat(
        &self,
        messages: Vec<Value>,
        schema: &Value,
    ) -> Result<Value, String>;
}

/// A mock `StructuredChat` for tests that returns a canned response.
pub struct MockChat {
    pub response: Value,
}

#[async_trait]
impl StructuredChat for MockChat {
    async fn structured_chat(
        &self,
        _messages: Vec<Value>,
        _schema: &Value,
    ) -> Result<Value, String> {
        Ok(self.response.clone())
    }
}

/// An optional judge the host can hand the engine -- BigTiny wires its
/// per-app decision model here (`bigtiny2_daemon::decider`). Both questions
/// are optimisations: `None` means "no opinion", and the engine then does
/// exactly what it would have done without a gate.
#[async_trait]
pub trait MemoryGate: Send + Sync {
    /// `Some(p)` when a learn pass over `chunk` should be skipped, with `p`
    /// the judged probability that it holds anything durable about the user
    /// (recorded in the audit log). `None` = learn as usual.
    async fn skip_learning(&self, chunk: &str) -> Option<f32>;

    /// How relevant each belief is to `message`, by position (`None` for a
    /// belief it gave no answer for), and the probability below which a
    /// belief should not be injected at all.
    async fn belief_relevance(
        &self,
        message: &str,
        beliefs: &[&str],
    ) -> Option<(Vec<Option<f32>>, f32)>;
}

/// A scripted `MemoryGate` for tests.
pub struct MockGate {
    /// Returned by `skip_learning`.
    pub skip: Option<f32>,
    /// Relevance by substring: a belief containing the key gets the value.
    pub relevance: Vec<(String, f32)>,
    pub drop_below: f32,
}

#[async_trait]
impl MemoryGate for MockGate {
    async fn skip_learning(&self, _chunk: &str) -> Option<f32> {
        self.skip
    }

    async fn belief_relevance(
        &self,
        _message: &str,
        beliefs: &[&str],
    ) -> Option<(Vec<Option<f32>>, f32)> {
        if self.relevance.is_empty() {
            return None;
        }
        let ps = beliefs
            .iter()
            .map(|b| {
                self.relevance
                    .iter()
                    .find(|(k, _)| b.contains(k.as_str()))
                    .map(|(_, p)| *p)
            })
            .collect();
        Some((ps, self.drop_below))
    }
}
