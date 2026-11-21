//! Deep research runs, over the engine's `/api/research` routes.
//!
//! Kitty owns none of a run's state: the engine drafts the plan in turn 0,
//! keeps the ledger, drives the waves, the critique and the write-up, and
//! reports progress on the app event stream (`research_progress`, forwarded as
//! `research://progress`). Kitty starts a run from the chat (`/deepresearch`),
//! shows the plan for approval, and renders progress and the report.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::bigtiny::client::BigTinyClient;
use crate::bigtiny::turn_text::InlineDocument;

/// What `/deepresearch` asks for, beside the brief itself.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ResearchArgs {
    /// Turns, counting the waves, the critique and the write-up. `None`: the
    /// brief's own number if it states one, else the engine's default (4).
    #[serde(default)]
    pub max_turns: Option<i64>,
}

/// What starting a run returns.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Started {
    pub research_id: String,
    pub max_turns: i64,
    /// Turn 0's instructions, which ride with the brief as a document.
    pub instructions: String,
}

/// The label turn 0's instructions carry in the chat.
pub const INSTRUCTIONS_LABEL: &str = "Deep research instructions";

/// Tie `session_id` to a new run. Refused (409) while the chat already has a
/// run in progress.
pub async fn start(
    client: &BigTinyClient,
    session_id: &str,
    brief: &str,
    args: &ResearchArgs,
) -> Result<Started, String> {
    let mut body = json!({"session_id": session_id, "brief": brief});
    if let Some(n) = args.max_turns {
        body["max_turns"] = json!(n);
    }
    let resp = client.post_json("/api/research", &body).await?;
    parse_started(&resp)
}

fn parse_started(resp: &Value) -> Result<Started, String> {
    serde_json::from_value(resp.clone()).map_err(|_| {
        resp.get("error")
            .and_then(Value::as_str)
            .unwrap_or("The engine did not start the research run.")
            .to_string()
    })
}

/// Turn 0's instructions as the document that rides with the brief.
pub fn instructions_document(started: &Started) -> InlineDocument {
    InlineDocument {
        label: INSTRUCTIONS_LABEL.to_string(),
        content: started.instructions.clone(),
        pinned: false,
    }
}

pub async fn get(client: &BigTinyClient, research_id: &str) -> Result<Value, String> {
    client
        .get_json(&format!("/api/research/{research_id}"))
        .await
}

pub async fn entries(client: &BigTinyClient, research_id: &str) -> Result<Value, String> {
    client
        .get_json(&format!("/api/research/{research_id}/entries"))
        .await
}

pub async fn cancel(client: &BigTinyClient, research_id: &str) -> Result<(), String> {
    client
        .post_json(&format!("/api/research/{research_id}/cancel"), &json!({}))
        .await
        .map(|_| ())
}

/// This app's runs, newest first; only one chat's when `session_id` is given.
pub async fn list(client: &BigTinyClient, session_id: Option<&str>) -> Result<Value, String> {
    let path = match session_id {
        Some(s) => format!("/api/apps/me/research?session_id={}", urlencode(s)),
        None => "/api/apps/me/research".to_string(),
    };
    let resp = client.get_json(&path).await?;
    Ok(resp.get("runs").cloned().unwrap_or_else(|| json!([])))
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_response_is_read_or_its_error_reported() {
        let ok = json!({"research_id": "r1", "status": "planning", "max_turns": 4, "instructions": "Plan."});
        let started = parse_started(&ok).unwrap();
        assert_eq!(started.max_turns, 4);
        let doc = instructions_document(&started);
        assert_eq!(doc.label, INSTRUCTIONS_LABEL);
        assert_eq!(doc.content, "Plan.");
        let err =
            parse_started(&json!({"error": "this chat already has a research run"})).unwrap_err();
        assert!(err.contains("already has"));
    }

    #[test]
    fn session_ids_are_escaped_in_queries() {
        assert_eq!(urlencode("abc-1_2"), "abc-1_2");
        assert_eq!(urlencode("a b&c"), "a%20b%26c");
    }
}
