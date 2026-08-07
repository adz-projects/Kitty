//! Trait that inverts the AP→daemon dependency. `adaptive_pathway` defines a
//! `StructuredChat` abstraction for "give me a JSON-schema-constrained chat
//! completion"; the daemon implements it over its `SummarizerClient`. This
//! lets the engine depend on a plain interface instead of a concrete daemon
//! type, resolving the circular dependency (orphan rule permits: foreign
//! trait + local type inside bigtiny_rust).

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
