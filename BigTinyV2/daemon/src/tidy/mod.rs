//! Tidying an app's chats: the figures a client grades its chats on, and the
//! state of one tidy run.
//!
//! The client (Kitty's `/tidy`) does the grading: it reads every top-level
//! chat's figures from `GET /api/apps/me/chats/stats`, scores them, and ties
//! the candidates to the chat that will discuss them
//! (`POST /api/chat/{id}/tidy`), each under a short ref (`c1`, `c2`, …). The
//! chat's model sees those refs and nothing it was not given: it proposes a
//! list through [`server`]'s `propose_tidy` (an always-ask tool, so the user
//! approves, revises or rejects it) and may read a candidate's first and last
//! exchange through `read_chats` only once the user has allowed it
//! (`POST /api/chat/{id}/tidy/readable`). Deleting is the client's: it owns
//! the chats' folders and its own bookkeeping.

pub mod server;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::error::StorageError;
use crate::storage::sessions;

/// The session-metadata key holding a tidy run's candidates and consents.
pub const META_TIDY: &str = "tidy";

/// Longest stretch of one message `read_chats` returns.
const EXCERPT_CHARS: usize = 800;

/// One top-level chat's figures, for grading.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ChatStats {
    pub id: String,
    pub name: Option<String>,
    pub created_at: Option<String>,
    /// When its newest message was written; `None` for a chat with none.
    pub last_message_at: Option<String>,
    pub user_turns: i64,
    /// The working folder, as the client set it.
    pub cwd: Option<String>,
    /// Set on a scheduled task's run.
    pub schedule_id: Option<String>,
    /// Every chat started under it (delegates, research steps), at any
    /// depth. They go with it.
    pub descendants: Vec<String>,
}

/// One row of [`chat_stats`]'s query: id, name, created, metadata, last
/// message, user turns.
type StatsRow = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
);

/// Every top-level chat `app_id` owns, with its figures.
pub async fn chat_stats(pool: &SqlitePool, app_id: &str) -> Result<Vec<ChatStats>, StorageError> {
    let rows: Vec<StatsRow> = sqlx::query_as(
        r#"SELECT s.id, s.name, CAST(s.created_at AS TEXT), s.metadata,
                      (SELECT CAST(MAX(m.created_at) AS TEXT) FROM messages m
                        WHERE m.session_id = s.id),
                      (SELECT COUNT(*) FROM messages m
                        WHERE m.session_id = s.id AND m.role = 'user')
               FROM sessions s
               WHERE s.app_id = ? AND s.parent_session_id IS NULL
               ORDER BY s.updated_at DESC"#,
    )
    .bind(app_id)
    .fetch_all(pool)
    .await?;
    let edges: Vec<(String, String)> = sqlx::query_as(
        r#"SELECT id, parent_session_id FROM sessions
           WHERE app_id = ? AND parent_session_id IS NOT NULL"#,
    )
    .bind(app_id)
    .fetch_all(pool)
    .await?;
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    for (id, parent) in edges {
        children.entry(parent).or_default().push(id);
    }
    Ok(rows
        .into_iter()
        .map(|(id, name, created_at, metadata, last, turns)| {
            let meta: Value = metadata
                .as_deref()
                .and_then(|m| serde_json::from_str(m).ok())
                .unwrap_or(Value::Null);
            let text = |k: &str| meta.get(k).and_then(Value::as_str).map(str::to_string);
            ChatStats {
                descendants: descendants_of(&id, &children),
                id,
                name,
                created_at,
                last_message_at: last,
                user_turns: turns,
                cwd: text("cwd"),
                schedule_id: text("schedule_id"),
            }
        })
        .collect())
}

/// Every chat under `root`, depth first. A cycle (which `set_parent` cannot
/// make, but a hand-edited database could) is walked once.
fn descendants_of(root: &str, children: &HashMap<String, Vec<String>>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut stack: Vec<&str> = vec![root];
    while let Some(id) = stack.pop() {
        for child in children.get(id).into_iter().flatten() {
            if child != root && !out.contains(child) {
                out.push(child.clone());
                stack.push(child);
            }
        }
    }
    out
}

/// One chat (or group of chats) the client put up for tidying. Fields beyond
/// these are the client's own and are kept as given.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Candidate {
    /// What the model calls it: `c1`, `c2`, ….
    #[serde(rename = "ref")]
    pub reference: String,
    /// The chats it stands for; the first is the one `read_chats` reads.
    pub session_ids: Vec<String>,
    #[serde(default)]
    pub title: String,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// A tidy run's state, as stored on its chat.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TidyState {
    pub candidates: Vec<Candidate>,
    /// Refs the user has allowed the model to read.
    #[serde(default)]
    pub readable: Vec<String>,
}

impl TidyState {
    pub fn candidate(&self, reference: &str) -> Option<&Candidate> {
        self.candidates.iter().find(|c| c.reference == reference)
    }
}

pub fn state_from_metadata(metadata: &Value) -> Option<TidyState> {
    serde_json::from_value(metadata.get(META_TIDY)?.clone()).ok()
}

pub async fn state_of(pool: &SqlitePool, session_id: &str) -> Option<TidyState> {
    let meta = sessions::get_session_metadata(pool, session_id)
        .await
        .ok()
        .flatten()?;
    let meta: Value = serde_json::from_str(&meta).ok()?;
    state_from_metadata(&meta)
}

/// Why a set of candidates was refused.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum CandidateError {
    #[error("a candidate has no ref or no chats")]
    Empty,
    #[error("ref {0} is used twice")]
    DuplicateRef(String),
    #[error("chat {0} is not one of this app's")]
    NotOwned(String),
    #[error("a chat cannot tidy itself")]
    SelfListed,
}

/// Check a client's candidates: unique refs, each naming chats `app_id` owns,
/// none of them the tidy chat itself.
pub async fn validate_candidates(
    pool: &SqlitePool,
    app_id: &str,
    tidy_chat: &str,
    candidates: &[Candidate],
) -> Result<Result<(), CandidateError>, StorageError> {
    let mut refs: Vec<&str> = Vec::new();
    for c in candidates {
        if c.reference.trim().is_empty() || c.session_ids.is_empty() {
            return Ok(Err(CandidateError::Empty));
        }
        if refs.contains(&c.reference.as_str()) {
            return Ok(Err(CandidateError::DuplicateRef(c.reference.clone())));
        }
        refs.push(&c.reference);
        for id in &c.session_ids {
            if id == tidy_chat {
                return Ok(Err(CandidateError::SelfListed));
            }
            if !sessions::is_owned_by(pool, id, app_id).await? {
                return Ok(Err(CandidateError::NotOwned(id.clone())));
            }
        }
    }
    Ok(Ok(()))
}

/// Tie `candidates` to the tidy chat, replacing any earlier set (and its
/// consents: a new list is a new question).
pub async fn set_candidates(
    pool: &SqlitePool,
    session_id: &str,
    candidates: Vec<Candidate>,
) -> Result<(), StorageError> {
    let state = TidyState {
        candidates,
        readable: Vec::new(),
    };
    let value = serde_json::to_value(&state)
        .map_err(|e| StorageError::Generic(format!("tidy state: {e}")))?;
    sessions::update_metadata_with(pool, session_id, move |mut meta| {
        if !meta.is_object() {
            meta = json!({});
        }
        meta[META_TIDY] = value;
        meta
    })
    .await
}

/// Record the user's consent to read `refs`. Returns the refs now readable;
/// a ref that is not a candidate is ignored.
pub async fn allow_reading(
    pool: &SqlitePool,
    session_id: &str,
    refs: &[String],
) -> Result<Option<Vec<String>>, StorageError> {
    let Some(mut state) = state_of(pool, session_id).await else {
        return Ok(None);
    };
    for r in refs {
        if state.candidate(r).is_some() && !state.readable.contains(r) {
            state.readable.push(r.clone());
        }
    }
    let readable = state.readable.clone();
    let value = serde_json::to_value(&state)
        .map_err(|e| StorageError::Generic(format!("tidy state: {e}")))?;
    sessions::update_metadata_with(pool, session_id, move |mut meta| {
        if !meta.is_object() {
            meta = json!({});
        }
        meta[META_TIDY] = value;
        meta
    })
    .await?;
    Ok(Some(readable))
}

/// A stored message's text: plain content, or a block message's text blocks.
fn message_text(content: Option<&str>, format: Option<&str>) -> String {
    let content = content.unwrap_or("");
    if format == Some("blocks") {
        if let Ok(Value::Array(blocks)) = serde_json::from_str::<Value>(content) {
            return blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("");
        }
    }
    content.to_string()
}

fn excerpt(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= EXCERPT_CHARS {
        return text.to_string();
    }
    let cut: String = text.chars().take(EXCERPT_CHARS).collect();
    format!("{}… [cut]", cut.trim_end())
}

/// One exchange: a user message and the first answer after it.
#[derive(Debug, Clone, PartialEq)]
pub struct Exchange {
    pub user: String,
    pub assistant: Option<String>,
}

/// A chat's first and last exchange (the same one when it has only one),
/// each message cut to a readable length. Empty for a chat with no user
/// message.
pub async fn first_and_last_exchange(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<Vec<Exchange>, StorageError> {
    let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        r#"SELECT role, content, content_format FROM messages
           WHERE session_id = ? AND role IN ('user', 'assistant')
           ORDER BY rowid"#,
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    let msgs: Vec<(bool, String)> = rows
        .into_iter()
        .map(|(role, c, f)| (role == "user", message_text(c.as_deref(), f.as_deref())))
        .filter(|(_, t)| !t.trim().is_empty())
        .collect();
    let exchange_at = |i: usize| Exchange {
        user: excerpt(&msgs[i].1),
        assistant: msgs[i + 1..]
            .iter()
            .take_while(|(user, _)| !user)
            .last()
            .map(|(_, t)| excerpt(t)),
    };
    let users: Vec<usize> = msgs
        .iter()
        .enumerate()
        .filter(|(_, (user, _))| *user)
        .map(|(i, _)| i)
        .collect();
    Ok(match (users.first(), users.last()) {
        (Some(&first), Some(&last)) if first == last => vec![exchange_at(first)],
        (Some(&first), Some(&last)) => vec![exchange_at(first), exchange_at(last)],
        _ => Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        crate::storage::apps::register_app(&pool, "kitty", "Kitty", "k1")
            .await
            .unwrap();
        crate::storage::apps::register_app(&pool, "other", "Other", "k2")
            .await
            .unwrap();
        pool
    }

    pub(crate) async fn message(pool: &SqlitePool, session: &str, role: &str, text: &str) {
        sqlx::query("INSERT INTO messages (id, session_id, role, content) VALUES (?, ?, ?, ?)")
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(session)
            .bind(role)
            .bind(text)
            .execute(pool)
            .await
            .unwrap();
    }

    pub(crate) fn candidate(reference: &str, ids: &[&str]) -> Candidate {
        Candidate {
            reference: reference.into(),
            session_ids: ids.iter().map(|s| s.to_string()).collect(),
            title: format!("title {reference}"),
            extra: Default::default(),
        }
    }

    #[tokio::test]
    async fn stats_count_turns_and_gather_background_chats() {
        let pool = pool().await;
        for (id, app) in [
            ("a", "kitty"),
            ("d1", "kitty"),
            ("d2", "kitty"),
            ("x", "other"),
        ] {
            sessions::create_session_for_app(&pool, id, id, app)
                .await
                .unwrap();
        }
        sessions::set_parent(&pool, "d1", "a", "kitty")
            .await
            .unwrap();
        sessions::set_parent(&pool, "d2", "d1", "kitty")
            .await
            .unwrap();
        message(&pool, "a", "user", "hi").await;
        message(&pool, "a", "assistant", "hello").await;
        message(&pool, "a", "user", "again").await;

        let stats = chat_stats(&pool, "kitty").await.unwrap();
        assert_eq!(stats.len(), 1, "background chats are not listed: {stats:?}");
        let a = &stats[0];
        assert_eq!(a.user_turns, 2);
        assert!(a.last_message_at.is_some());
        let mut d = a.descendants.clone();
        d.sort();
        assert_eq!(d, ["d1", "d2"]);
    }

    #[tokio::test]
    async fn candidates_must_be_the_apps_own_and_not_the_tidy_chat() {
        let pool = pool().await;
        for (id, app) in [("t", "kitty"), ("a", "kitty"), ("x", "other")] {
            sessions::create_session_for_app(&pool, id, id, app)
                .await
                .unwrap();
        }
        let ok = [candidate("c1", &["a"])];
        assert_eq!(
            validate_candidates(&pool, "kitty", "t", &ok).await.unwrap(),
            Ok(())
        );
        let theirs = [candidate("c1", &["x"])];
        assert_eq!(
            validate_candidates(&pool, "kitty", "t", &theirs)
                .await
                .unwrap(),
            Err(CandidateError::NotOwned("x".into()))
        );
        let itself = [candidate("c1", &["t"])];
        assert_eq!(
            validate_candidates(&pool, "kitty", "t", &itself)
                .await
                .unwrap(),
            Err(CandidateError::SelfListed)
        );
        let twice = [candidate("c1", &["a"]), candidate("c1", &["a"])];
        assert!(matches!(
            validate_candidates(&pool, "kitty", "t", &twice)
                .await
                .unwrap(),
            Err(CandidateError::DuplicateRef(_))
        ));
    }

    #[tokio::test]
    async fn consent_is_recorded_per_ref_and_reset_by_a_new_list() {
        let pool = pool().await;
        sessions::create_session_for_app(&pool, "t", "t", "kitty")
            .await
            .unwrap();
        set_candidates(
            &pool,
            "t",
            vec![candidate("c1", &["a"]), candidate("c2", &["b"])],
        )
        .await
        .unwrap();
        let now = allow_reading(&pool, "t", &["c2".into(), "c9".into()])
            .await
            .unwrap();
        assert_eq!(
            now,
            Some(vec!["c2".to_string()]),
            "an unknown ref is ignored"
        );
        set_candidates(&pool, "t", vec![candidate("c1", &["a"])])
            .await
            .unwrap();
        assert!(state_of(&pool, "t").await.unwrap().readable.is_empty());
    }

    #[tokio::test]
    async fn first_and_last_exchange_skip_the_middle() {
        let pool = pool().await;
        sessions::create_session_for_app(&pool, "a", "a", "kitty")
            .await
            .unwrap();
        for (role, text) in [
            ("user", "first question"),
            ("assistant", "first answer"),
            ("user", "middle"),
            ("assistant", "middle answer"),
            ("user", "last question"),
        ] {
            message(&pool, "a", role, text).await;
        }
        let ex = first_and_last_exchange(&pool, "a").await.unwrap();
        assert_eq!(ex.len(), 2);
        assert_eq!(ex[0].user, "first question");
        assert_eq!(ex[0].assistant.as_deref(), Some("first answer"));
        assert_eq!(ex[1].user, "last question");
        assert_eq!(ex[1].assistant, None);

        let long = "x".repeat(EXCERPT_CHARS + 50);
        sessions::create_session_for_app(&pool, "b", "b", "kitty")
            .await
            .unwrap();
        message(&pool, "b", "user", &long).await;
        let ex = first_and_last_exchange(&pool, "b").await.unwrap();
        assert_eq!(ex.len(), 1);
        assert!(ex[0].user.ends_with("[cut]"));
    }
}
