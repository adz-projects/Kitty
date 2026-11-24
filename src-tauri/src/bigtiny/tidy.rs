//! `/tidy`: suggest which old chats to delete, for the user to approve.
//!
//! Kitty grades every chat from figures alone (turns, time since the last
//! message, files kept in the chat's own folder, a folder bookmark, a missing
//! title) and hands the chat's model the likely candidates under short refs
//! (`c1`, …). The model proposes a list through the engine's `propose_tidy`,
//! an always-ask tool, so the user approves, revises or rejects it on the
//! tidy card; it may read a candidate's first and last exchange
//! (`read_chats`) only once the user has allowed that chat on the card, which
//! the engine enforces (`BigTinyV2/daemon/src/tidy/`). On approval Kitty
//! deletes the chats (each with its background chats and its own folder),
//! then the tidy chat itself. Deleting is permanent.
//!
//! Scheduled runs are graded as one group per schedule, the newest run left
//! out of it, since Settings links to it.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use crate::bigtiny::client::BigTinyClient;
use crate::bigtiny::turn_text::InlineDocument;
use crate::state::AppState;

/// The label the candidate list carries in the chat.
pub const CANDIDATES_LABEL: &str = "Tidy candidates";

/// A chat whose last message is newer than this is never a candidate: it may
/// still be in use.
const RECENT_DAYS: i64 = 2;
/// At or above: Kitty suggests deleting it.
const DELETE_AT: i32 = 50;
/// At or above (and below [`DELETE_AT`]): worth a second look.
const UNSURE_AT: i32 = 25;
/// Most candidates put to the model; the highest-scoring first.
const MAX_CANDIDATES: usize = 150;
/// Files counted in a chat folder before Kitty stops looking.
const FILE_COUNT_CAP: usize = 50;

/// One top-level chat's figures, as the engine reports them
/// (`GET /api/apps/me/chats/stats`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ChatStats {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub last_message_at: Option<String>,
    #[serde(default)]
    pub user_turns: i64,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub schedule_id: Option<String>,
    #[serde(default)]
    pub descendants: Vec<String>,
}

/// What Kitty knows about one chat (or one schedule's runs) beyond the
/// engine's figures.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    pub title: String,
    /// The chats it stands for, the one to read first; background chats
    /// last.
    pub session_ids: Vec<String>,
    pub user_turns: i64,
    pub last_activity: Option<DateTime<Utc>>,
    /// Files kept in the chat's own folder (never a working folder the
    /// user chose, whose files are not the chat's).
    pub files: usize,
    pub folder: Option<String>,
    /// Background chats (delegates, research steps) that go with it.
    pub background: usize,
    /// Set for a group of scheduled runs: how many.
    pub runs: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Suggestion {
    Delete,
    Unsure,
    Keep,
}

/// A grade: the score, Kitty's suggestion, and the reasons a person reads.
#[derive(Debug, Clone, PartialEq)]
pub struct Grade {
    pub score: i32,
    pub suggestion: Suggestion,
    pub reasons: Vec<String>,
}

pub fn is_untitled(title: &str) -> bool {
    let t = title.trim().to_lowercase();
    t.is_empty() || t == "new chat" || t == "untitled" || t == "untitled chat"
}

fn ago(days: i64) -> String {
    match days {
        0 => "today".into(),
        1 => "yesterday".into(),
        d if d < 60 => format!("{d} days ago"),
        d if d < 730 => format!("{} months ago", d / 30),
        d => format!("{} years ago", d / 365),
    }
}

/// Grade one chat. `None` for a chat too recent to judge.
pub fn grade(facts: &Facts, now: DateTime<Utc>) -> Option<Grade> {
    let days = facts
        .last_activity
        .map(|t| (now - t).num_days().max(0))
        .unwrap_or(i64::MAX / 2);
    if days < RECENT_DAYS {
        return None;
    }
    let mut score = 0;
    let mut reasons = Vec::new();

    let (turn_points, turn_reason) = match (facts.runs, facts.user_turns) {
        (Some(n), _) => (30, format!("{n} scheduled runs")),
        (None, 0) => (40, "no messages".to_string()),
        (None, 1) => (25, "1 turn".to_string()),
        (None, n @ 2..=3) => (15, format!("{n} turns")),
        (None, n @ 4..=9) => (0, format!("{n} turns")),
        (None, n) => (-20, format!("{n} turns")),
    };
    score += turn_points;
    reasons.push(turn_reason);

    score += match days {
        d if d < 7 => -10,
        d if d < 30 => 0,
        d if d < 90 => 10,
        d if d < 180 => 20,
        _ => 30,
    };
    reasons.push(match facts.last_activity {
        Some(_) => format!("last message {}", ago(days)),
        None => "never used".into(),
    });

    if facts.runs.is_none() && is_untitled(&facts.title) {
        score += 25;
        reasons.push("untitled".into());
    }
    if facts.files > 0 {
        score -= 60;
        reasons.push(match facts.files {
            1 => "1 file in its folder".to_string(),
            n if n >= FILE_COUNT_CAP => format!("{n}+ files in its folder"),
            n => format!("{n} files in its folder"),
        });
    }
    if let Some(folder) = &facts.folder {
        score -= 15;
        reasons.push(format!("in the folder \u{201c}{folder}\u{201d}"));
    }
    if facts.background > 0 {
        reasons.push(match facts.background {
            1 => "1 background chat goes with it".to_string(),
            n => format!("{n} background chats go with it"),
        });
    }
    let suggestion = if score >= DELETE_AT {
        Suggestion::Delete
    } else if score >= UNSURE_AT {
        Suggestion::Unsure
    } else {
        Suggestion::Keep
    };
    Some(Grade {
        score,
        suggestion,
        reasons,
    })
}

/// An engine timestamp: RFC 3339, or SQLite's `YYYY-MM-DD HH:MM:SS[.f]`
/// (UTC).
pub fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    for fmt in ["%Y-%m-%d %H:%M:%S%.f%:z", "%Y-%m-%d %H:%M:%S%.f%#z"] {
        if let Ok(t) = DateTime::parse_from_str(s, fmt) {
            return Some(t.with_timezone(&Utc));
        }
    }
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
        .ok()
        .map(|t| t.and_utc())
}

/// The chats as graded units: one per chat, and one per schedule for its
/// runs (all but the newest, which stays). `files_in` counts the files kept
/// in a chat's own folder; `folder_of` names its folder bookmark.
pub fn facts_from_stats(
    chats: &[ChatStats],
    exclude: &[String],
    files_in: impl Fn(&ChatStats) -> usize,
    folder_of: impl Fn(&str) -> Option<String>,
) -> Vec<Facts> {
    let activity = |c: &ChatStats| {
        c.last_message_at
            .as_deref()
            .or(c.created_at.as_deref())
            .and_then(parse_time)
    };
    let mut out = Vec::new();
    let mut schedules: BTreeMap<&str, Vec<&ChatStats>> = BTreeMap::new();
    for c in chats.iter().filter(|c| !exclude.contains(&c.id)) {
        if let Some(sid) = c.schedule_id.as_deref() {
            schedules.entry(sid).or_default().push(c);
            continue;
        }
        let mut ids = vec![c.id.clone()];
        ids.extend(c.descendants.iter().cloned());
        out.push(Facts {
            title: c.name.clone().unwrap_or_default(),
            session_ids: ids,
            user_turns: c.user_turns,
            last_activity: activity(c),
            files: files_in(c),
            folder: folder_of(&c.id),
            background: c.descendants.len(),
            runs: None,
        });
    }
    for runs in schedules.into_values() {
        let mut runs = runs;
        runs.sort_by_key(|c| std::cmp::Reverse(activity(c)));
        let older = &runs[1..];
        if older.is_empty() {
            continue;
        }
        let name = runs[0]
            .name
            .as_deref()
            .map(|n| n.strip_prefix("scheduled: ").unwrap_or(n))
            .unwrap_or("a scheduled task");
        let mut ids: Vec<String> = older.iter().map(|c| c.id.clone()).collect();
        let background: Vec<String> = older
            .iter()
            .flat_map(|c| c.descendants.iter().cloned())
            .collect();
        let bg = background.len();
        ids.extend(background);
        out.push(Facts {
            title: format!("Older runs of \u{201c}{name}\u{201d}"),
            session_ids: ids,
            user_turns: older.iter().map(|c| c.user_turns).sum(),
            last_activity: older.iter().filter_map(|c| activity(c)).max(),
            files: older.iter().map(|c| files_in(c)).sum(),
            folder: older.iter().find_map(|c| folder_of(&c.id)),
            background: bg,
            runs: Some(older.len()),
        });
    }
    out
}

/// One candidate as the engine stores it and the card reads it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Candidate {
    #[serde(rename = "ref")]
    pub reference: String,
    pub session_ids: Vec<String>,
    pub title: String,
    pub user_turns: i64,
    pub last_activity: Option<String>,
    pub files: usize,
    pub folder: Option<String>,
    pub background: usize,
    pub runs: Option<usize>,
    pub suggestion: Suggestion,
    pub score: i32,
    pub reasons: Vec<String>,
}

/// The candidates (Kitty's delete and unsure suggestions, best first) and
/// how many chats were left out as clear keeps or too recent.
pub fn candidates(facts: &[Facts], now: DateTime<Utc>) -> (Vec<Candidate>, usize) {
    let mut graded: Vec<(&Facts, Grade)> = facts
        .iter()
        .filter_map(|f| grade(f, now).map(|g| (f, g)))
        .filter(|(_, g)| g.suggestion != Suggestion::Keep)
        .collect();
    graded.sort_by(|a, b| {
        b.1.score
            .cmp(&a.1.score)
            .then(a.0.last_activity.cmp(&b.0.last_activity))
    });
    graded.truncate(MAX_CANDIDATES);
    let kept = facts.len() - graded.len();
    let out = graded
        .into_iter()
        .enumerate()
        .map(|(i, (f, g))| Candidate {
            reference: format!("c{}", i + 1),
            session_ids: f.session_ids.clone(),
            title: if f.title.trim().is_empty() {
                "New Chat".into()
            } else {
                f.title.clone()
            },
            user_turns: f.user_turns,
            last_activity: f.last_activity.map(|t| t.to_rfc3339()),
            files: f.files,
            folder: f.folder.clone(),
            background: f.background,
            runs: f.runs,
            suggestion: g.suggestion,
            score: g.score,
            reasons: g.reasons,
        })
        .collect();
    (out, kept)
}

fn cell(s: &str) -> String {
    s.replace(['\n', '\r'], " ").replace('|', "/")
}

/// The turn's document: the role, the rules, the user's wishes, and the
/// candidates as a table of refs.
pub fn document(candidates: &[Candidate], kept: usize, wishes: &str) -> InlineDocument {
    let mut s = String::new();
    s.push_str(
        "You are helping the user tidy their chats. Kitty graded every chat on figures alone \
         (turns, time since the last message, files kept in the chat's folder, a folder \
         bookmark, a missing title) and lists the likely candidates below by ref. You have \
         not read any of them.\n\n\
         Your job: decide which to delete and which you are unsure about, then call \
         propose_tidy once with those refs (every other listed chat is kept). The user then \
         approves, revises or rejects the list on a card; on approval Kitty deletes the chats \
         permanently, and this chat too.\n\n\
         Rules:\n\
         - Start from Kitty's suggestion; move a chat only for a reason in its row or the \
         user's wishes, and give that reason in `reasons`.\n\
         - When a title alone could hide something worth keeping, list it as unsure with what \
         reading it would settle. Never ask to read every chat.\n\
         - You may read a chat (read_chats: its first and last exchange) only after the user \
         allows it from the card. Never claim to know what a chat contains otherwise.\n\
         - Write nothing before calling propose_tidy but, at most, one sentence.\n\n",
    );
    if !wishes.trim().is_empty() {
        s.push_str(&format!("The user's wishes: {}\n\n", wishes.trim()));
    }
    if candidates.is_empty() {
        s.push_str(
            "Kitty found nothing worth tidying. Say so in one sentence and do not call \
             propose_tidy.\n",
        );
    } else {
        s.push_str(
            "| ref | title | turns | last message | files | folder | Kitty suggests | why |\n\
             |---|---|---|---|---|---|---|---|\n",
        );
        for c in candidates {
            let suggestion = match c.suggestion {
                Suggestion::Delete => "delete",
                Suggestion::Unsure => "unsure",
                Suggestion::Keep => "keep",
            };
            s.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
                c.reference,
                cell(&c.title),
                c.user_turns,
                c.last_activity
                    .as_deref()
                    .map(|t| t.get(..10).unwrap_or(t))
                    .unwrap_or("never"),
                c.files,
                c.folder.as_deref().map(cell).unwrap_or_default(),
                suggestion,
                cell(&c.reasons.join("; ")),
            ));
        }
    }
    s.push_str(&format!(
        "\nChats not listed (clear keeps, or too recent to judge): {kept}.\n"
    ));
    InlineDocument {
        label: CANDIDATES_LABEL.to_string(),
        content: s,
        pinned: false,
    }
}

/// Files in a chat's own folder (skipping dotfiles), counted up to a cap.
/// Zero for a working folder the user chose: its files are not the chat's.
fn files_in_chat_folder(app: &AppHandle, cwd: Option<&str>) -> usize {
    let Some(cwd) = cwd.filter(|c| !c.is_empty()) else {
        return 0;
    };
    if !crate::commands::is_default_folder(app, cwd) {
        return 0;
    }
    let mut count = 0;
    let mut stack = vec![Path::new(cwd).to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => {
                    count += 1;
                    if count >= FILE_COUNT_CAP {
                        return count;
                    }
                }
                _ => {}
            }
        }
    }
    count
}

pub async fn chat_stats(client: &BigTinyClient) -> Result<Vec<ChatStats>, String> {
    let resp = client.get_json("/api/apps/me/chats/stats").await?;
    serde_json::from_value(resp.get("chats").cloned().unwrap_or_else(|| json!([])))
        .map_err(|e| format!("chat stats: {e}"))
}

/// Grade every chat and tie the candidates to `session_id`. Returns the
/// document that rides with the user's message.
pub async fn prepare(
    app: &AppHandle,
    session_id: &str,
    wishes: &str,
) -> Result<InlineDocument, String> {
    let client = crate::bigtiny::client::ensure_client(app)?;
    let stats = chat_stats(&client).await?;
    let (folders, busy) = {
        let state = app.state::<AppState>();
        let folders = state.config.lock().unwrap().session_folders.clone();
        let busy: Vec<String> = state
            .in_flight_sessions
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect();
        (folders, busy)
    };
    let mut exclude = busy;
    exclude.push(session_id.to_string());
    let app2 = app.clone();
    let stats2 = stats.clone();
    let facts = tokio::task::spawn_blocking(move || {
        facts_from_stats(
            &stats2,
            &exclude,
            |c| files_in_chat_folder(&app2, c.cwd.as_deref()),
            |id| folders.get(id).cloned(),
        )
    })
    .await
    .map_err(|e| format!("grading chats: {e}"))?;
    let (cands, kept) = candidates(&facts, Utc::now());
    client
        .post_json(
            &format!("/api/chat/{session_id}/tidy"),
            &json!({ "candidates": cands }),
        )
        .await?;
    Ok(document(&cands, kept, wishes))
}

/// The tidy chat's candidates and consents, for the card.
pub async fn state(client: &BigTinyClient, session_id: &str) -> Result<Value, String> {
    client
        .get_json(&format!("/api/chat/{session_id}/tidy"))
        .await
}

/// Let the model read `refs`, and send it back to read them and propose
/// again.
pub async fn allow_reading(
    app: &AppHandle,
    session_id: &str,
    action_id: &str,
    refs: &[String],
) -> Result<(), String> {
    let client = crate::bigtiny::client::ensure_client(app)?;
    client
        .post_json(
            &format!("/api/chat/{session_id}/tidy/readable"),
            &json!({ "refs": refs }),
        )
        .await?;
    let message = format!(
        "The user allowed you to read {}. Read them with read_chats, decide each, then call \
         propose_tidy again.",
        refs.join(", ")
    );
    crate::lifecycle::app_events::approve(
        app,
        session_id,
        action_id,
        "revise",
        None,
        Some(&message),
    )
    .await
}

/// What approving did.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Applied {
    pub deleted: usize,
    pub failed: usize,
}

/// Delete the chats behind `refs` (with their background chats and their own
/// folders), then the tidy chat itself.
pub async fn apply(
    app: &AppHandle,
    session_id: &str,
    action_id: &str,
    refs: &[String],
) -> Result<Applied, String> {
    let client = crate::bigtiny::client::ensure_client(app)?;
    let tidy: Value = state(&client, session_id).await?;
    let candidates = tidy
        .get("candidates")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut targets: Vec<String> = Vec::new();
    for c in &candidates {
        let reference = c.get("ref").and_then(Value::as_str).unwrap_or("");
        if !refs.iter().any(|r| r == reference) {
            continue;
        }
        for id in c
            .get("session_ids")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if id != session_id && !targets.iter().any(|t| t == id) {
                targets.push(id.to_string());
            }
        }
    }
    // Folders come from the figures, read now: a chat's cwd decides whether
    // its folder is Kitty's to remove (`delete_session` checks again).
    let cwds: HashMap<String, String> = chat_stats(&client)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|c| c.cwd.map(|cwd| (c.id, cwd)))
        .collect();

    let mut applied = Applied {
        deleted: 0,
        failed: 0,
    };
    for id in &targets {
        match crate::commands::delete_session(app.clone(), id.clone(), cwds.get(id).cloned()).await
        {
            Ok(()) => applied.deleted += 1,
            Err(e) => {
                tracing::warn!("tidy: could not delete {id}: {e}");
                applied.failed += 1;
            }
        }
    }
    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        let before = cfg.session_folders.len();
        cfg.session_folders
            .retain(|id, _| !targets.contains(id) && id != session_id);
        if cfg.session_folders.len() != before {
            if let Err(e) = crate::config::save(&cfg) {
                tracing::warn!("tidy: could not save folders: {e}");
            }
        }
    }

    // Close the proposal, stop the turn, and remove the tidy chat.
    let own_cwd = crate::bigtiny::sessions::metadata(&client, session_id)
        .await
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_string);
    let _ = crate::lifecycle::app_events::approve(app, session_id, action_id, "allow", None, None)
        .await;
    let _ = crate::bigtiny::stream::cancel(app, session_id).await;
    if let Err(e) =
        crate::commands::delete_session(app.clone(), session_id.to_string(), own_cwd).await
    {
        tracing::warn!("tidy: could not delete the tidy chat {session_id}: {e}");
    }
    let _ = app.emit(
        "tidy://done",
        json!({ "session_id": session_id, "deleted": applied.deleted, "failed": applied.failed }),
    );
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn now() -> DateTime<Utc> {
        parse_time("2026-11-24T12:00:00Z").unwrap()
    }

    fn facts(title: &str, turns: i64, days: i64) -> Facts {
        Facts {
            title: title.into(),
            session_ids: vec!["s".into()],
            user_turns: turns,
            last_activity: Some(now() - Duration::days(days)),
            ..Default::default()
        }
    }

    fn suggestion(f: &Facts) -> Option<Suggestion> {
        grade(f, now()).map(|g| g.suggestion)
    }

    #[test]
    fn engine_timestamps_parse_in_both_shapes() {
        assert!(parse_time("2026-11-24 10:00:00").is_some());
        assert!(parse_time("2026-11-24 10:00:00.123").is_some());
        assert!(parse_time("2026-11-24T10:00:00+00:00").is_some());
        assert!(parse_time("2026-11-24 10:00:00+00:00").is_some());
        assert!(parse_time("yesterday").is_none());
    }

    #[test]
    fn grading_follows_turns_age_title_and_files() {
        // Recent chats are never judged.
        assert_eq!(suggestion(&facts("New Chat", 0, 1)), None);
        // Empty and untitled: delete.
        assert_eq!(
            suggestion(&facts("New Chat", 0, 3)),
            Some(Suggestion::Delete)
        );
        assert_eq!(
            suggestion(&facts("Untitled", 1, 10)),
            Some(Suggestion::Delete)
        );
        // One titled turn, old: delete; two months: a second look.
        assert_eq!(
            suggestion(&facts("B550M socket", 1, 200)),
            Some(Suggestion::Delete)
        );
        assert_eq!(
            suggestion(&facts("B550M socket", 1, 60)),
            Some(Suggestion::Unsure)
        );
        // A long conversation is kept, however old.
        assert_eq!(
            suggestion(&facts("Op-ed drafts", 12, 400)),
            Some(Suggestion::Keep)
        );
        // Files in its folder outweigh age and brevity.
        let mut with_files = facts("New Chat", 1, 300);
        with_files.files = 3;
        assert_eq!(suggestion(&with_files), Some(Suggestion::Keep));
        // A folder nudges, but does not decide.
        let mut in_folder = facts("New Chat", 0, 300);
        in_folder.folder = Some("Work".into());
        assert_eq!(suggestion(&in_folder), Some(Suggestion::Delete));
        let mut nudged = facts("Socket question", 1, 200);
        nudged.folder = Some("Work".into());
        assert_eq!(suggestion(&nudged), Some(Suggestion::Unsure));
    }

    #[test]
    fn reasons_say_why_in_words() {
        let mut f = facts("New Chat", 1, 120);
        f.background = 2;
        let g = grade(&f, now()).unwrap();
        assert_eq!(
            g.reasons,
            [
                "1 turn",
                "last message 4 months ago",
                "untitled",
                "2 background chats go with it"
            ]
        );
    }

    fn stat(id: &str, name: &str, last: &str, schedule: Option<&str>) -> ChatStats {
        ChatStats {
            id: id.into(),
            name: Some(name.into()),
            created_at: Some("2025-01-01 00:00:00".into()),
            last_message_at: Some(last.into()),
            user_turns: 1,
            cwd: None,
            schedule_id: schedule.map(str::to_string),
            descendants: vec![],
        }
    }

    #[test]
    fn scheduled_runs_group_by_schedule_and_keep_the_newest() {
        let chats = vec![
            stat("r1", "scheduled: Brief", "2026-01-01 08:00:00", Some("sch")),
            stat("r2", "scheduled: Brief", "2026-02-01 08:00:00", Some("sch")),
            stat("r3", "scheduled: Brief", "2026-03-01 08:00:00", Some("sch")),
            stat(
                "solo",
                "scheduled: Once",
                "2026-03-01 08:00:00",
                Some("one"),
            ),
            stat("tidy", "New Chat", "2026-03-01 08:00:00", None),
            {
                let mut c = stat("p", "Parent", "2026-03-01 08:00:00", None);
                c.descendants = vec!["d1".into()];
                c
            },
        ];
        let facts = facts_from_stats(&chats, &["tidy".into()], |_| 0, |_| None);
        assert_eq!(facts.len(), 2, "{facts:?}");
        let group = facts.iter().find(|f| f.runs.is_some()).unwrap();
        assert_eq!(group.title, "Older runs of \u{201c}Brief\u{201d}");
        assert_eq!(group.session_ids, ["r2", "r1"], "the newest run stays");
        let parent = facts.iter().find(|f| f.title == "Parent").unwrap();
        assert_eq!(parent.session_ids, ["p", "d1"]);
        assert_eq!(parent.background, 1);
    }

    #[test]
    fn candidates_are_refs_in_score_order_and_the_document_lists_them() {
        let all = vec![
            facts("Keep me", 20, 400),
            facts("Socket question", 1, 60),
            facts("New Chat", 0, 30),
        ];
        let (cands, kept) = candidates(&all, now());
        assert_eq!(kept, 1);
        assert_eq!(cands[0].title, "New Chat");
        assert_eq!(cands[0].reference, "c1");
        assert_eq!(cands[1].reference, "c2");
        let doc = document(&cands, kept, "keep anything about the op-ed");
        assert_eq!(doc.label, CANDIDATES_LABEL);
        assert!(
            doc.content.contains("| c1 | New Chat | 0 |"),
            "{}",
            doc.content
        );
        assert!(doc.content.contains("keep anything about the op-ed"));
        assert!(doc.content.contains("too recent to judge): 1."));
        let empty = document(&[], 3, "");
        assert!(empty.content.contains("do not call"));
    }

    #[test]
    fn untitled_means_no_real_title() {
        assert!(is_untitled(""));
        assert!(is_untitled(" New Chat "));
        assert!(is_untitled("untitled"));
        assert!(!is_untitled("New chat about GPUs"));
    }
}
