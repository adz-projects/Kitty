//! The specialists Kitty itself defines, and the built-ins it retires.
//!
//! Both are app-scoped writes to `/api/specialists`, so other apps sharing the
//! engine keep their own view:
//!
//! - `debater`, the participant in a `/debate` (`commands::session::debate`).
//!   Its host is named by the moderator per call (`host_policy: "caller"`), so
//!   the engine runs it only in a turn Kitty armed with a debate panel.
//! - `extractor` and `analyst` are switched off for Kitty by saving disabled
//!   copies of them, once. A user who switches one back on keeps it on.
//!
//! Run from the self-heal pass that follows each built-in MCP sync, because
//! the engine refuses a definition whose tools no connected server provides:
//! before kitty-tools and kitty-web are up, the debater cannot be written.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::json;
use tauri::{AppHandle, Manager};

use super::client::BigTinyClient;
use super::specialists::{self, Specialist, SpecialistSpec};
use crate::state::AppState;

pub const DEBATER: &str = "debater";

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

/// What to write, given the app's current view and whether the built-ins were
/// already retired once. Pure, for the tests.
fn plan(list: &[Specialist], retired: bool) -> Vec<SpecialistSpec> {
    let mut writes = Vec::new();
    if !list.iter().any(|s| s.name == DEBATER && !s.builtin) {
        writes.push(debater_spec());
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

/// Create the debater if missing, and retire the built-ins once. Best-effort:
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
        assert_eq!(names, ["debater", "extractor", "analyst"]);
        assert!(!writes[1].enabled && !writes[2].enabled);
        assert_eq!(
            writes[1].fan_out.as_deref(),
            Some("per_ref"),
            "the copy is faithful"
        );
        assert_eq!(writes[0].host_policy.as_deref(), Some("caller"));
    }

    #[test]
    fn nothing_is_rewritten_once_done() {
        let list = vec![spec_row("debater", false), spec_row("extractor", true)];
        assert!(
            plan(&list, true).is_empty(),
            "a re-enabled built-in stays as the user left it"
        );
        // An app copy already there is the user's decision, retired or not.
        let list = vec![spec_row("debater", false), spec_row("extractor", false)];
        assert!(plan(&list, false).is_empty());
    }
}
