//! `/critique`: the editor's playbook and the panel a critique turn carries.
//!
//! Like `/debate`, the critique is run by the chat's own model, acting as the
//! editor; the knowledge and the instructions come from the shared
//! `kitty-critique` crate. A critique spans two turns: the `/critique` turn,
//! where the editor reads the draft and proposes a brief, and the user's
//! answer to it, where the critique runs. Both are armed with the panel, so
//! the editor can hand each lens to a fresh `critic` delegate on a host of its
//! choosing; the playbook rides only on the first, stored with the message
//! (so the prompt prefix stays cached) and read back from history on the
//! second. The brief turn is not armed: there is nothing to delegate yet.
//!
//! When the chat cannot delegate (specialists off, Android, no usable host),
//! the editor applies every lens itself, and the guides are inlined in the
//! playbook instead of written to the chat's folder.

use std::path::Path;

use kitty_critique::{Delegation, GuideSource, Host, PlaybookInput};
use tauri::{AppHandle, Manager};

use crate::bigtiny::debate::{chat_dir, panel_seats, write_item, Seat};
use crate::bigtiny::turn_text::{InlineDocument, CRITIQUE_LABEL};
use crate::state::AppState;

/// Which turn of a critique this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The `/critique` turn: the editor proposes the brief.
    Brief,
    /// The user's answer to the brief: the critique runs.
    Run,
}

impl Stage {
    /// From the webview's `send_prompt` argument.
    pub fn parse(s: &str) -> Option<Stage> {
        match s {
            "start" => Some(Stage::Brief),
            "continue" => Some(Stage::Run),
            _ => None,
        }
    }
}

/// What a critique turn adds to an ordinary one.
pub struct CritiqueTurn {
    /// The playbook, sent as the turn's last inline document. Only on the
    /// brief turn.
    pub playbook: Option<InlineDocument>,
    /// Host ids to arm for this turn; `None` when the editor works alone.
    pub panel: Option<Vec<String>>,
}

/// The folder, inside the chat's own, holding the draft and the guides.
const FOLDER: &str = "critique";

/// Build what a critique turn carries. `text` is what the user typed after
/// `/critique` (its options are read from it); `documents` and
/// `attached_paths` are the turn's pastes and dropped files.
pub async fn prepare(
    app: &AppHandle,
    session_id: &str,
    stage: Stage,
    text: &str,
    documents: &[InlineDocument],
    attached_paths: &[String],
) -> CritiqueTurn {
    let (seats, can_delegate) = panel_seats(app, session_id).await;
    let delegating = can_delegate && !seats.is_empty();
    let panel = delegating.then(|| seats.iter().map(|s| s.host.id.clone()).collect());
    if stage == Stage::Run {
        return CritiqueTurn {
            playbook: None,
            panel,
        };
    }

    // Delegates read the draft and the guides from the chat's folder, which
    // they inherit the grant to. Working alone, the editor needs neither on
    // disk: the pasted draft is in its message and the guides are inlined.
    let dir = if delegating {
        chat_dir(app, session_id).await
    } else {
        None
    };
    let mut drafts: Vec<String> = attached_paths.to_vec();
    let mut guides = GuideSource::Inline;
    if let Some(dir) = &dir {
        let mut unsaved = false;
        for doc in documents {
            match write_item(dir, FOLDER, doc) {
                Ok(path) => drafts.push(path.to_string_lossy().into_owned()),
                Err(e) => {
                    tracing::warn!("could not save a critique draft: {e}");
                    unsaved = true;
                }
            }
        }
        if unsaved {
            drafts.push("the pasted document in the user's message".into());
        }
        match write_guides(&dir.join(FOLDER).join("guides")) {
            Ok(folder) => guides = GuideSource::Folder(folder),
            Err(e) => tracing::warn!("could not write the critique guides: {e}"),
        }
    }

    let author_context = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        cfg.critique.author_context.clone()
    };
    // Delegates cannot read guides that exist only in the playbook.
    let delegation = if delegating && guides != GuideSource::Inline {
        Delegation::Panel(hosts(&seats))
    } else {
        Delegation::Solo
    };
    let playbook = kitty_critique::playbook(&PlaybookInput {
        options: kitty_critique::parse_options(text),
        delegation,
        guides,
        drafts: &drafts,
        author_context: Some(author_context.as_str()),
        // Relative: every chat's system prompt already says that files the
        // user asked for go in its `outputs/` folder.
        outputs_dir: Some("outputs/"),
    });
    CritiqueTurn {
        playbook: Some(InlineDocument {
            label: CRITIQUE_LABEL.into(),
            content: playbook,
            pinned: false,
        }),
        panel: None,
    }
}

fn hosts(seats: &[Seat]) -> Vec<Host> {
    seats
        .iter()
        .map(|s| Host {
            id: s.host.id.clone(),
            label: s.label(),
        })
        .collect()
}

/// Write every guide into `folder`, rewriting only those that changed, and
/// return the folder as the playbook names it.
fn write_guides(folder: &Path) -> std::io::Result<String> {
    std::fs::create_dir_all(folder)?;
    for guide in kitty_critique::guides() {
        let path = folder.join(guide.file);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(guide.content) {
            std::fs::write(&path, guide.content)?;
        }
    }
    Ok(folder.to_string_lossy().into_owned())
}

/// For "Regenerate": the panel to arm again when the question being asked
/// again is the answer to a critique's brief, that is when `previous`, the
/// user turn before it, was the `/critique` turn. `None` otherwise (the brief
/// turn itself is never armed).
pub async fn panel_for_regenerate(
    app: &AppHandle,
    session_id: &str,
    previous: Option<&str>,
) -> Option<Vec<String>> {
    if !previous.is_some_and(crate::bigtiny::turn_text::is_critique_turn) {
        return None;
    }
    prepare(app, session_id, Stage::Run, "", &[], &[])
        .await
        .panel
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_webview_names_the_two_stages() {
        assert_eq!(Stage::parse("start"), Some(Stage::Brief));
        assert_eq!(Stage::parse("continue"), Some(Stage::Run));
        assert_eq!(Stage::parse("debate"), None);
    }

    #[test]
    fn guides_are_written_once_and_kept_current() {
        let dir = std::env::temp_dir().join(format!("kitty-critique-{}", std::process::id()));
        let folder = write_guides(&dir).unwrap();
        assert_eq!(Path::new(&folder), dir);
        let core = dir.join(kitty_critique::CORE.file);
        assert_eq!(
            std::fs::read_to_string(&core).unwrap(),
            kitty_critique::CORE.content
        );
        std::fs::write(&core, "stale").unwrap();
        write_guides(&dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(&core).unwrap(),
            kitty_critique::CORE.content,
            "an edited or older guide is replaced"
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            kitty_critique::guides().len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
