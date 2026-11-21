//! The specialists Kitty itself defines, and the built-ins it retires.
//!
//! Both are app-scoped writes to `/api/specialists`, so other apps sharing the
//! engine keep their own view:
//!
//! - `debater`, the participant in a `/debate` (`commands::session::debate`).
//!   Its host is named by the moderator per call (`host_policy: "caller"`), so
//!   the engine runs it only in a turn Kitty armed with a debate panel.
//! - `critic`, which applies one lens of a `/critique` (`bigtiny::critique`),
//!   also caller-hosted: the editor puts each lens on a host of the panel
//!   Kitty armed for that turn.
//! - `extractor` and `analyst` are switched off for Kitty by saving disabled
//!   copies of them, once. A user who switches one back on keeps it on.
//!
//! Run from the self-heal pass that follows each built-in MCP sync, because
//! the engine refuses a definition whose tools no connected server provides:
//! before kitty-tools and kitty-web are up, the debater and the critic cannot be written.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::json;
use tauri::{AppHandle, Manager};

use super::client::BigTinyClient;
use super::specialists::{self, Specialist, SpecialistSpec};
use crate::state::AppState;

pub const DEBATER: &str = "debater";
pub const CRITIC: &str = kitty_critique::CRITIC;

/// Built-ins Kitty does not offer its model.
pub const RETIRED_BUILTINS: [&str; 2] = ["extractor", "analyst"];

/// Done for this run of Kitty. Cleared never: the debater is created only
/// when missing, so once it exists there is nothing left to do until restart.
static DONE: AtomicBool = AtomicBool::new(false);

/// The tools a debater may use: read the item under debate, or a page it was
/// given. Nothing that writes, runs or searches.
const DEBATER_TOOLS: [&str; 6] = [
    "lean_file_read",
    "lean_pdf_read_text",
    "lean_word_read_text",
    "lean_doc_read_chunk",
    "lean_doc_search",
    "lean_web_scrape",
];

pub fn debater_spec() -> SpecialistSpec {
    SpecialistSpec {
        name: DEBATER.into(),
        description: "One participant in a debate the user started with /debate. Give it its \
                      persona, the item under debate (a path or URL), the user's instructions \
                      for the debate, and the transcript so far, and name its `host` from the \
                      debate panel. It argues as that persona and nothing else."
            .into(),
        system_prompt: "You are one participant in a structured debate. The request gives you \
your persona, the item under debate, what the user wants the debate to examine, and the \
transcript so far.\n\n\
- Stay in your persona throughout. Argue from its own position, pressures and constraints, \
not as a neutral commentator, and never step outside it to summarise the debate.\n\
- Ground what you say in the item under debate. Read it with your tools when you were given a \
path or URL; quote or point to the parts you are responding to.\n\
- Answer the earlier speakers by name: agree, push back, or build on what they actually said. \
In the first round, when there is no transcript yet, open with your own position.\n\
- Keep to what the user asked the debate to examine. Be concrete and brief: one contribution, \
not an essay."
            .into(),
        provider: None,
        model: None,
        tool_allow: DEBATER_TOOLS.iter().map(|t| t.to_string()).collect(),
        response_schema: Some(json!({
            "type": "object",
            "properties": {
                "persona": {"type": "string", "description": "Your persona's name."},
                "responds_to": {
                    "type": "array",
                    "description": "The speakers you respond to by name. Empty in round 1.",
                    "items": {"type": "string"}
                },
                "position": {
                    "type": "string",
                    "description": "Your position in one sentence."
                },
                "statement": {
                    "type": "string",
                    "description": "What you say in this round, in your persona's voice."
                }
            },
            "required": ["persona", "responds_to", "position", "statement"],
            "additionalProperties": false
        })),
        max_steps: Some(8),
        reasoning_cap_tokens: None,
        reasoning_cap_fraction: None,
        fan_out: None,
        max_concurrent: None,
        host_policy: Some("caller".into()),
        enabled: true,
    }
}

/// The tools a critic may use: read the draft and its guides, and search and
/// read the web for a fact-check. Nothing that writes or runs.
const CRITIC_TOOLS: [&str; 8] = [
    "lean_file_read",
    "lean_pdf_read_text",
    "lean_word_read_text",
    "lean_doc_read_chunk",
    "lean_doc_search",
    "lean_web_scrape",
    "lean_web_search",
    "lean_web_search_read_chunk",
];

pub fn critic_spec() -> SpecialistSpec {
    SpecialistSpec {
        name: CRITIC.into(),
        description: "Applies one lens of a writing critique the user started with /critique.                       Give it the task named as its guide names it, the confirmed brief, the                       map once there is one, and in `refs` the draft and the guide files; name                       its `host` from the panel. It reports findings and never rewrites the                       draft, except a sketch when the task is one."
            .into(),
        system_prompt: "You are one reader on an editorial team critiquing a draft. The request names your task (a lens, the map, a fact-check batch, or a sketch), the brief the author agreed, and the map of the draft once it exists. Your sources are the draft and the guide files.

- Read the draft in full, and the section of the guide that defines your task. Follow that section exactly; do nothing outside it.
- Ground every finding in a paragraph number from the map and a verbatim quote from the draft. Report the most consequential findings first, at most eight.
- Never invent facts, sources or experiences for the author. Where a fix needs what only the author knows, ask the question that would surface it.
- Unless your task is a sketch, diagnose only: do not rewrite the draft.
- For a fact-check, decide every verdict from a source you actually read."
            .into(),
        provider: None,
        model: None,
        tool_allow: CRITIC_TOOLS.iter().map(|t| t.to_string()).collect(),
        response_schema: Some(json!({
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "Your task, as the request named it."},
                "report": {
                    "type": "string",
                    "description": "Your findings, map, verdicts or sketch, in markdown, in the                                     format the guide asks for."
                }
            },
            "required": ["task", "report"],
            "additionalProperties": false
        })),
        max_steps: Some(20),
        reasoning_cap_tokens: None,
        reasoning_cap_fraction: None,
        fan_out: None,
        max_concurrent: None,
        host_policy: Some("caller".into()),
        enabled: true,
    }
}

/// What to write, given the app's current view and whether the built-ins were
/// already retired once. Pure, for the tests.
fn plan(list: &[Specialist], retired: bool) -> Vec<SpecialistSpec> {
    let mut writes = Vec::new();
    if !list.iter().any(|s| s.name == DEBATER && !s.builtin) {
        writes.push(debater_spec());
    }
    if !list.iter().any(|s| s.name == CRITIC && !s.builtin) {
        writes.push(critic_spec());
    }
    if !retired {
        for name in RETIRED_BUILTINS {
            // Only a built-in still in force: an app copy means the user (or an
            // earlier run) already decided.
            if let Some(b) = list.iter().find(|s| s.name == name && s.builtin) {
                let mut spec = SpecialistSpec::from(b);
                spec.enabled = false;
                writes.push(spec);
            }
        }
    }
    writes
}

/// Create the debater and the critic if missing, and retire the built-ins once. Best-effort:
/// a failure is retried at the next self-heal pass.
pub async fn ensure(app: &AppHandle, client: &BigTinyClient) {
    if DONE.load(Ordering::Relaxed) {
        return;
    }
    let retired = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        cfg.specialists.builtins_retired
    };
    let list = match specialists::list(client).await {
        Ok(list) => list,
        Err(e) => {
            tracing::debug!("could not list specialists yet: {e}");
            return;
        }
    };
    let mut ok = true;
    for spec in plan(&list, retired) {
        if let Err(e) = specialists::save(client, &spec).await {
            tracing::warn!("could not write the {} specialist: {e}", spec.name);
            ok = false;
        }
    }
    if !ok {
        return;
    }
    if !retired {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        cfg.specialists.builtins_retired = true;
        if let Err(e) = crate::config::save(&cfg) {
            tracing::warn!("could not record the retired specialists: {e}");
        }
    }
    DONE.store(true, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_row(name: &str, builtin: bool) -> Specialist {
        serde_json::from_value(json!({
            "id": name, "name": name, "description": "d", "system_prompt": "s",
            "tool_allow": ["lean_file_read"], "max_steps": 5, "fan_out": "per_ref",
            "enabled": true, "builtin": builtin
        }))
        .unwrap()
    }

    #[test]
    fn a_fresh_install_gets_the_debater_and_retires_two_builtins() {
        let list = vec![
            spec_row("researcher", true),
            spec_row("extractor", true),
            spec_row("analyst", true),
        ];
        let writes = plan(&list, false);
        let names: Vec<_> = writes.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["debater", "critic", "extractor", "analyst"]);
        assert!(!writes[2].enabled && !writes[3].enabled);
        assert_eq!(writes[1].host_policy.as_deref(), Some("caller"));
        assert_eq!(
            writes[2].fan_out.as_deref(),
            Some("per_ref"),
            "the copy is faithful"
        );
        assert_eq!(writes[0].host_policy.as_deref(), Some("caller"));
    }

    #[test]
    fn nothing_is_rewritten_once_done() {
        let list = vec![
            spec_row("debater", false),
            spec_row("critic", false),
            spec_row("extractor", true),
        ];
        assert!(
            plan(&list, true).is_empty(),
            "a re-enabled built-in stays as the user left it"
        );
        // An app copy already there is the user's decision, retired or not.
        let list = vec![
            spec_row("debater", false),
            spec_row("critic", false),
            spec_row("extractor", false),
        ];
        assert!(plan(&list, false).is_empty());
        // An install from before /critique gets the critic alone.
        let list = vec![spec_row("debater", false), spec_row("extractor", false)];
        let names: Vec<_> = plan(&list, true).into_iter().map(|w| w.name).collect();
        assert_eq!(names, ["critic"]);
    }
}
