//! Specialist definitions — thin wrappers over `/api/specialists`.
//!
//! Kitty owns none of this data. A specialist lives in the daemon because the
//! daemon is what runs it, and because the model reaches the same definitions
//! through `call_specialist` without Kitty in the loop at all. Kitty's job is
//! the authoring UI and the model picker, so these are deliberately thin:
//! anything that looks like policy (which tools are legal, what the answer must
//! look like, how many may run at once) is enforced daemon-side, where the
//! model-driven path goes through it too.
//!
//! Replaces `config::recipes`, which stored client-side prompt templates in
//! Kitty's own `config.json` and invoked them by a `/slug` the user had to
//! learn. Nothing here is invoked by the user directly.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::bigtiny::client::BigTinyClient;

/// One definition, as the daemon reports it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Specialist {
    pub id: String,
    pub name: String,
    /// What the calling model reads when deciding whether to delegate. The
    /// single most consequential field in the form.
    pub description: String,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub tool_allow: Vec<String>,
    #[serde(default)]
    pub response_schema: Option<Value>,
    #[serde(default)]
    pub max_steps: i64,
    /// The daemon reports the reasoning cap as one tagged value; the form
    /// edits it as two exclusive fields, filled from this by `list`.
    #[serde(default, skip_serializing)]
    reasoning_cap: Option<ReasoningCap>,
    #[serde(default)]
    pub reasoning_cap_tokens: Option<i32>,
    #[serde(default)]
    pub reasoning_cap_fraction: Option<f64>,
    /// `per_ref` splits one call with N refs into N delegates.
    #[serde(default)]
    pub fan_out: Option<String>,
    /// How many runs of this specialist may run at once; unset is the
    /// daemon's global limit only.
    #[serde(default)]
    pub max_concurrent: Option<i64>,
    #[serde(default)]
    pub enabled: bool,
    /// Seeded by the daemon and shared by every app. Editable only by defining
    /// one of the same name, which shadows it; never deletable.
    #[serde(default)]
    pub builtin: bool,
}

/// The daemon's `agent::tokens::ReasoningCap`, as it serializes.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReasoningCap {
    Tokens(i32),
    ContextFraction(f64),
}

impl Specialist {
    /// Spread the daemon's tagged cap onto the two fields the form edits.
    fn with_flat_cap(mut self) -> Self {
        match self.reasoning_cap.take() {
            Some(ReasoningCap::Tokens(n)) => self.reasoning_cap_tokens = Some(n),
            Some(ReasoningCap::ContextFraction(f)) => self.reasoning_cap_fraction = Some(f),
            None => {}
        }
        self
    }
}

/// A definition being written. Mirrors the daemon's `WriteRequest`; `id` is
/// absent because the daemon keys on `name` — writing an existing name edits
/// that definition, and writing a built-in's name creates this app's override
/// of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecialistSpec {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub tool_allow: Vec<String>,
    #[serde(default)]
    pub response_schema: Option<Value>,
    #[serde(default)]
    pub max_steps: Option<i64>,
    /// Absolute reasoning-token cap; wins over the fraction when both are set.
    #[serde(default)]
    pub reasoning_cap_tokens: Option<i32>,
    #[serde(default)]
    pub reasoning_cap_fraction: Option<f64>,
    #[serde(default)]
    pub fan_out: Option<String>,
    #[serde(default)]
    pub max_concurrent: Option<i64>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

pub async fn list(client: &BigTinyClient) -> Result<Vec<Specialist>, String> {
    let resp = client.get_json("/api/specialists").await?;
    Ok(parse_list(&resp))
}

fn parse_list(resp: &Value) -> Vec<Specialist> {
    let rows = resp
        .get("specialists")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    rows.into_iter()
        .filter_map(|r| serde_json::from_value::<Specialist>(r).ok())
        .map(Specialist::with_flat_cap)
        .collect()
}

pub async fn save(client: &BigTinyClient, spec: &SpecialistSpec) -> Result<String, String> {
    let resp = client
        .post_json(
            "/api/specialists",
            &serde_json::to_value(spec).map_err(|e| e.to_string())?,
        )
        .await?;
    resp.get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "daemon did not return a specialist id".to_string())
}

/// Delete this app's own definition. A built-in answers 403 — the daemon's
/// refusal message already explains that overriding is a save, not a delete, so
/// it is surfaced verbatim rather than reworded here.
pub async fn delete(client: &BigTinyClient, id: &str) -> Result<(), String> {
    client.delete(&format!("/api/specialists/{id}")).await?;
    Ok(())
}

/// Every tool name this app can currently reach, sorted and de-duplicated.
///
/// Aggregated across the app's visible MCP servers rather than exposed
/// per-server: a specialist's `tool_allow` is a flat list of names (the
/// daemon's tool registry is deliberately flat and un-namespaced), so which
/// server provides a tool is not a distinction the form can meaningfully
/// offer. A server that fails to answer is skipped rather than failing the
/// whole list — a disconnected server should narrow the checklist, not empty
/// it.
pub async fn available_tools(client: &BigTinyClient) -> Result<Vec<String>, String> {
    let servers = crate::bigtiny::mcp::list_servers(client).await?;
    let mut names: Vec<String> = Vec::new();
    for server in servers.iter().filter(|s| s.enabled) {
        let Ok(resp) = client
            .get_json(&format!("/api/mcp/servers/{}/tools", server.id))
            .await
        else {
            continue;
        };
        if let Some(tools) = resp.get("tools").and_then(|v| v.as_array()) {
            names.extend(
                tools
                    .iter()
                    .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
                    .map(str::to_string),
            );
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// One recorded delegate run, from `GET /api/specialists/runs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecialistRun {
    pub id: String,
    pub specialist: Option<String>,
    /// The delegate's own session, so its transcript stays reachable when the
    /// structured report was not enough.
    pub session_id: Option<String>,
    pub status: String,
    pub started_at: Option<String>,
    pub summary: Option<String>,
    /// Where the run actually ran, which the fallback chain may have moved
    /// away from the specialist's pin.
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

pub async fn runs(client: &BigTinyClient) -> Result<Vec<SpecialistRun>, String> {
    let resp = client.get_json("/api/specialists/runs").await?;
    let rows = resp
        .get("runs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(rows
        .into_iter()
        .filter_map(|r| serde_json::from_value(r).ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every field the form edits reaches the daemon (#20): these were dropped
    /// by serde before, so the form saved them and nothing happened.
    #[test]
    fn a_spec_carries_every_field_the_form_edits() {
        let input = json!({
            "name": "triage", "description": "d", "system_prompt": "s",
            "provider": null, "model": null, "tool_allow": ["lean_web_search"],
            "response_schema": null, "max_steps": 8,
            "reasoning_cap_tokens": 2048, "reasoning_cap_fraction": null,
            "fan_out": "per_ref", "max_concurrent": 2, "enabled": true
        });
        let spec: SpecialistSpec = serde_json::from_value(input.clone()).unwrap();
        let out = serde_json::to_value(&spec).unwrap();
        for key in [
            "reasoning_cap_tokens",
            "fan_out",
            "max_concurrent",
            "max_steps",
        ] {
            assert_eq!(out[key], input[key], "{key} must round-trip");
        }
    }

    #[test]
    fn the_daemons_tagged_cap_becomes_the_forms_fields() {
        let resp = json!({ "specialists": [
            { "id": "1", "name": "a", "description": "", "reasoning_cap": { "tokens": 900 },
              "fan_out": "per_ref", "max_concurrent": 3 },
            { "id": "2", "name": "b", "description": "",
              "reasoning_cap": { "context_fraction": 0.25 } },
            { "id": "3", "name": "c", "description": "" }
        ]});
        let list = parse_list(&resp);
        assert_eq!(list[0].reasoning_cap_tokens, Some(900));
        assert_eq!(list[0].fan_out.as_deref(), Some("per_ref"));
        assert_eq!(list[0].max_concurrent, Some(3));
        assert_eq!(list[1].reasoning_cap_fraction, Some(0.25));
        assert_eq!(list[2].reasoning_cap_tokens, None);
        let shown = serde_json::to_value(&list[0]).unwrap();
        assert!(shown.get("reasoning_cap").is_none());
        assert_eq!(shown["reasoning_cap_tokens"], 900);
    }

    #[test]
    fn a_run_reports_where_it_ran() {
        let run: SpecialistRun = serde_json::from_value(json!({
            "id": "r", "specialist": "a", "session_id": null, "status": "done",
            "started_at": null, "summary": null,
            "provider_id": "prov-1", "model": "m"
        }))
        .unwrap();
        assert_eq!(run.provider_id.as_deref(), Some("prov-1"));
        assert_eq!(run.model.as_deref(), Some("m"));
    }
}
