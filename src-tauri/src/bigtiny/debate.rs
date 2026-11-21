//! `/debate`: the moderator brief and the panel a debate turn carries.
//!
//! The moderator is the chat's own model. Kitty gives it, inside the turn's
//! text (so it is stored with the message and the prompt prefix stays cached),
//! the rules of the debate and the panel of hosts it may seat personas on; and
//! it arms that panel in the engine for this turn only, which is what lets the
//! `debater` specialist run on those hosts. With fewer than two hosts, or with
//! specialists off, there is nothing to arm: the brief instead tells the model
//! to voice every persona itself.

use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager};

use crate::bigtiny::turn_text::{InlineDocument, DEBATE_LABEL};
use crate::config::providers::debate::PanelHost;
use crate::state::AppState;

/// What a `/debate` turn adds to an ordinary one.
pub struct DebateTurn {
    /// The moderator brief, sent as the turn's last inline document.
    pub brief: InlineDocument,
    /// Host ids to arm for this turn; `None` when the model debates alone.
    pub panel: Option<Vec<String>>,
}

/// A panel host with what the moderator should know to choose between them.
/// Shared with `/critique` (`bigtiny::critique`), which arms the same panel.
pub(crate) struct Seat {
    pub(crate) host: PanelHost,
    /// 0–100 from the OpenRouter catalog, when it knows the model.
    capability: Option<i32>,
    /// `$`, `$$` or `$$$` by output price, likewise.
    price: Option<&'static str>,
}

impl Seat {
    /// `name — provider / model — capability, price`, as briefs list a host.
    pub(crate) fn label(&self) -> String {
        let capability = self.capability.map_or_else(
            || "capability unknown".to_string(),
            |c| format!("capability {c}"),
        );
        let price = self.price.unwrap_or("price unknown");
        format!(
            "{} — {} / {} — {capability}, {price}",
            self.host.name, self.host.provider_name, self.host.model
        )
    }
}

/// The panel's hosts, and whether this chat may delegate at all: specialists
/// are a desktop feature, switched on globally and not off for this chat.
pub(crate) async fn panel_seats(app: &AppHandle, session_id: &str) -> (Vec<Seat>, bool) {
    let (seats, enabled) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        let catalog = state.openrouter_catalog.lock().unwrap();
        let entries = catalog
            .as_ref()
            .map(|c| c.entries.as_slice())
            .unwrap_or(&[]);
        let seats: Vec<Seat> = cfg
            .debate_hosts()
            .into_iter()
            .map(|host| {
                let entry = crate::openrouter::catalog::match_in_catalog(&host.model, entries);
                Seat {
                    capability: entry
                        .and_then(|e| e.agentic_index.or(e.coding_index).or(e.intelligence_index))
                        .map(|r| r.clamp(0.0, 100.0) as i32),
                    price: entry.and_then(|e| e.cost_tier).map(|t| match t {
                        crate::openrouter::catalog::CostTier::Economy => "$",
                        crate::openrouter::catalog::CostTier::Moderate => "$$",
                        crate::openrouter::catalog::CostTier::Premium => "$$$",
                    }),
                    host,
                }
            })
            .collect();
        (seats, cfg.specialists.enabled)
    };
    let can_delegate = cfg!(desktop)
        && enabled
        && !crate::bigtiny::sessions::specialists_disabled(app, session_id).await;
    (seats, can_delegate)
}

const RULES: &str = "\
You are the moderator of a debate the user started with /debate. You do not argue yourself.

1. From the user's instructions, work out the personas, the number of rounds (3 if they do not \
say) and what the debate should examine.
2. Give each persona a different host from the panel below, preferring the more capable ones. \
Reuse a host only when there are more personas than hosts. A persona keeps its host for the \
whole debate.
3. Round 1: call `call_specialist` with specialist \"debater\" once per persona, all in the \
same step, each with its `host`. Then wait for every report with `await_specialists`.
4. Then call `debate_order` once, with every persona's name and the total number of rounds.
5. Every later round: follow that round's order exactly. Call one debater, wait for its report, \
then call the next, so each speaker hears the earlier rounds and everyone before them in this \
round.
6. Every debater request contains, in this order: (a) its persona, as the user described it; \
(b) the item under debate, with its paths or URLs also passed in `refs`; (c) the user's \
instructions for the debate; (d) the transcript so far, each statement labelled with its \
speaker and round.
7. If a debater fails, note it in the transcript and carry on.
8. After the last round, give the user the debate in brief, round by round, and then what they \
asked the debate to show.";

const SOLO: &str = "\
The user asked for a debate with /debate, but only one model is available to take part, so \
there is no panel. Hold the debate yourself: voice every persona in turn, round by round, as the \
user's instructions describe (3 rounds if they do not say), letting each speaker answer the ones \
before. Do not call specialists for it. End with what the user asked the debate to show.";

/// Build the brief and the panel for a `/debate` turn.
///
/// `documents` are the turn's inlined pastes and drops, and `attached_paths`
/// its dropped files. Pasted documents are also written into the chat's own
/// folder so the moderator can hand debaters a path instead of copying the
/// text into every request.
pub async fn prepare(
    app: &AppHandle,
    session_id: &str,
    documents: &[InlineDocument],
    attached_paths: &[String],
) -> DebateTurn {
    let (seats, can_delegate) = panel_seats(app, session_id).await;

    if !can_delegate || seats.len() < 2 {
        return DebateTurn {
            brief: brief(SOLO.to_string()),
            panel: None,
        };
    }

    let mut items: Vec<String> = attached_paths.to_vec();
    if !documents.is_empty() {
        if let Some(dir) = chat_dir(app, session_id).await {
            for doc in documents {
                match write_item(&dir, "debate", doc) {
                    Ok(path) => items.push(path.to_string_lossy().into_owned()),
                    Err(e) => tracing::warn!("could not save a debate item: {e}"),
                }
            }
        }
    }

    let panel: Vec<String> = seats.iter().map(|s| s.host.id.clone()).collect();
    DebateTurn {
        brief: brief(compose_brief(&seats, &items)),
        panel: Some(panel),
    }
}

fn brief(content: String) -> InlineDocument {
    InlineDocument {
        label: DEBATE_LABEL.into(),
        content,
        pinned: false,
    }
}

fn compose_brief(seats: &[Seat], items: &[String]) -> String {
    let mut out = String::from(RULES);
    out.push_str("\n\nPanel (host — name — provider / model — capability, price):\n");
    for s in seats {
        out.push_str(&format!("- {} — {}\n", s.host.id, s.label()));
    }
    out.push_str("\nItem under debate: ");
    if items.is_empty() {
        out.push_str(
            "whatever the user's message names or links. If it names nothing, ask the user \
             what to debate before starting.",
        );
    } else {
        out.push('\n');
        for item in items {
            out.push_str(&format!("- {item}\n"));
        }
    }
    out
}

/// The chat's own folder, where a pasted draft is saved for the debaters.
/// Delegates inherit the chat's folder grants, so they can read it there.
pub(crate) async fn chat_dir(app: &AppHandle, session_id: &str) -> Option<PathBuf> {
    let client = crate::bigtiny::client::ensure_client(app).ok()?;
    let meta = crate::bigtiny::sessions::metadata(&client, session_id).await;
    ["chat_dir", "cwd"]
        .iter()
        .find_map(|k| meta.get(*k).and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Save a pasted document as `<chat>/<folder>/<label>.md`, numbering it when
/// that name is taken by a different text.
pub(crate) fn write_item(
    dir: &Path,
    folder: &str,
    doc: &InlineDocument,
) -> std::io::Result<PathBuf> {
    let folder = dir.join(folder);
    std::fs::create_dir_all(&folder)?;
    let stem = safe_stem(&doc.label);
    for n in 0..100 {
        let name = if n == 0 {
            format!("{stem}.md")
        } else {
            format!("{stem} ({n}).md")
        };
        let path = folder.join(name);
        match std::fs::read_to_string(&path) {
            Ok(existing) if existing == doc.content => return Ok(path),
            Ok(_) => continue,
            Err(_) => {
                std::fs::write(&path, &doc.content)?;
                return Ok(path);
            }
        }
    }
    Err(std::io::Error::other("too many items with that name"))
}

/// A file name from a document label: path separators and characters Windows
/// refuses become `_`, and an extension already on the label is dropped.
fn safe_stem(label: &str) -> String {
    let base = Path::new(label)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(label);
    let cleaned: String = base
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() {
        "item".into()
    } else {
        cleaned
    }
}

/// For "Regenerate" on a `/debate` turn: the panel to arm again, from the
/// current settings. `None` when there is no debate to arm.
pub async fn panel_for_regenerate(
    app: &AppHandle,
    session_id: &str,
    stored: &str,
) -> Option<Vec<String>> {
    if !crate::bigtiny::turn_text::is_debate_turn(stored) {
        return None;
    }
    prepare(app, session_id, &[], &[]).await.panel
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(id: &str, capability: Option<i32>) -> Seat {
        Seat {
            host: PanelHost {
                id: id.into(),
                name: format!("{id} name"),
                provider_name: "OpenRouter".into(),
                model: format!("vendor/{id}"),
            },
            capability,
            price: capability.map(|_| "$$$"),
        }
    }

    #[test]
    fn the_brief_names_every_host_and_the_items() {
        let text = compose_brief(
            &[seat("debate-a", Some(80)), seat("debate-b", None)],
            &["C:/chats/1/debate/draft.md".into()],
        );
        assert!(text.contains("debate_order"));
        assert!(text.contains(
            "- debate-a — debate-a name — OpenRouter / vendor/debate-a — capability 80, $$$"
        ));
        assert!(text.contains("debate-b") && text.contains("capability unknown, price unknown"));
        assert!(text.contains("- C:/chats/1/debate/draft.md"));
    }

    #[test]
    fn a_pasted_item_is_saved_once_under_a_safe_name() {
        let dir = std::env::temp_dir().join(format!("kitty-debate-{}", std::process::id()));
        let doc = |content: &str| InlineDocument {
            label: "Pasted: a/b?.txt".into(),
            content: content.into(),
            pinned: false,
        };
        let first = write_item(&dir, "debate", &doc("one")).unwrap();
        assert_eq!(
            write_item(&dir, "debate", &doc("one")).unwrap(),
            first,
            "same text, same file"
        );
        let second = write_item(&dir, "debate", &doc("two")).unwrap();
        assert_ne!(first, second);
        assert!(first
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".md"));
        assert!(!first.file_name().unwrap().to_string_lossy().contains('?'));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
