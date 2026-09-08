//! What a specialist *is*, independent of how it is stored or run.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A named delegate agent.
///
/// Deliberately not a prompt template. The caller supplies the request in its
/// own words; a specialist supplies the standing instructions, the bounded tool
/// set, the model, and the shape of the answer. That split is what lets the
/// model choose a specialist from an ordinary user request instead of the user
/// learning an invocation syntax.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Specialist {
    pub id: String,
    /// `None` for a built-in, which every app can see. See migration 021.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    pub name: String,
    /// The routing text. This is the only thing the calling model reads when
    /// deciding whether to delegate, so it should say what this specialist is
    /// *for* and, where it matters, what it is not for.
    pub description: String,
    pub system_prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Exact tool names. Enforced at dispatch, not merely used to filter what
    /// the delegate is offered.
    pub tool_allow: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<Value>,
    pub max_steps: i64,
    /// How much of a run may go on reasoning. `None` uses the daemon default
    /// (`agent.specialist_reasoning_fraction`) — which is why delegates are
    /// capped by default without every definition having to say so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_cap: Option<crate::agent::tokens::ReasoningCap>,
    /// `Some("per_ref")` splits one call with N refs into N delegates. `None`
    /// runs a single delegate over all of them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fan_out: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<i64>,
    pub enabled: bool,
    /// Seeded by the daemon. Editable by an app only in the sense that the app
    /// may define its own specialist of the same name, which shadows this one.
    pub builtin: bool,
}
