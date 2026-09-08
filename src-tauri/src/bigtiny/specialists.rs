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
use serde_json::{json, Value};

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
    #[serde(default)]
    pub enabled: bool,
    /// Seeded by the daemon and shared by every app. Editable only by defining
    /// one of the same name, which shadows it; never deletable.
    #[serde(default)]
    pub builtin: bool,
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
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

pub async fn list(client: &BigTinyClient) -> Result<Vec<Specialist>, String> {
    let resp = client.get_json("/api/specialists").await?;
    let rows = resp
        .get("specialists")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(rows
        .into_iter()
        .filter_map(|r| serde_json::from_value(r).ok())
        .collect())
}

pub async fn save(client: &BigTinyClient, spec: &SpecialistSpec) -> Result<String, String> {
    let resp = client
        .post_json("/api/specialists", &serde_json::to_value(spec).map_err(|e| e.to_string())?)
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

/// Run one on the user's behalf, under `session_id`.
///
/// Long, because it awaits the delegate's whole run: the daemon holds the
/// request open until the specialist finishes, so this goes through
/// `post_json_long` rather than the ordinary client timeout.
pub async fn run(
    client: &BigTinyClient,
    name: &str,
    request: &str,
    refs: &[String],
    session_id: &str,
) -> Result<Value, String> {
    let body = json!({
        "request": request,
        "refs": refs,
        "session_id": session_id,
    });
    client
        .post_json_long(&format!("/api/specialists/{name}/run"), &body)
        .await
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
