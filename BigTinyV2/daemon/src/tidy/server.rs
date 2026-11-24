//! The in-process MCP server a tidy chat's model works through:
//! `propose_tidy` (always-ask: the user approves, revises or rejects the
//! list) and `read_chats` (a candidate's first and last exchange, only once
//! the user has allowed it).
//!
//! Like `research`, the connection is daemon-lifetime and shared, so the
//! calling session is injected into each call by `agent::loop_`
//! ([`TIDY_TOOLS`]), and both tools are withheld from every session that is
//! not a tidy chat (`agent::loop_::withhold_tidy_tools`).

use std::collections::BTreeMap;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;

use super::{first_and_last_exchange, state_of, TidyState};

pub const PROPOSE_TOOL: &str = "propose_tidy";
pub const READ_TOOL: &str = "read_chats";

/// Every tool this server owns. Both need the calling session injected.
pub const TIDY_TOOLS: [&str; 2] = [PROPOSE_TOOL, READ_TOOL];

/// Most candidates one `read_chats` call reads.
const MAX_READ: usize = 12;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProposeRequest {
    /// Refs (`c1`, …) of the chats to delete.
    #[serde(default)]
    pub delete: Vec<String>,
    /// Refs you cannot judge from the figures alone: the user may let you
    /// read their first and last exchange.
    #[serde(default)]
    pub unsure: Vec<String>,
    /// A short reason for any ref where yours differs from the listed one,
    /// or for an unsure ref: what reading it would settle.
    #[serde(default)]
    pub reasons: BTreeMap<String, String>,
    /// One or two sentences on the list as a whole.
    #[serde(default)]
    pub summary: Option<String>,
    /// Host-injected, never model-supplied.
    #[serde(default)]
    #[schemars(skip)]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadRequest {
    /// Refs (`c1`, …) the user allowed you to read.
    pub refs: Vec<String>,
    #[serde(default)]
    #[schemars(skip)]
    pub session_id: Option<String>,
}

#[derive(Clone)]
pub struct TidyServer {
    pool: SqlitePool,
    tool_router: ToolRouter<Self>,
}

fn refuse(message: impl Into<String>) -> String {
    json!({"ok": false, "error": message.into()}).to_string()
}

impl TidyServer {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            tool_router: Self::core_tool_router(),
        }
    }

    pub async fn serve_in_process<S>(self, stream: S) -> Result<(), String>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
    {
        let server = self.serve(stream).await.map_err(|e| e.to_string())?;
        server
            .waiting()
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn state(&self, session_id: Option<&str>) -> Result<TidyState, String> {
        let session_id = session_id
            .filter(|s| !s.is_empty())
            .ok_or("this chat is not tidying")?;
        state_of(&self.pool, session_id)
            .await
            .ok_or_else(|| "this chat is not tidying".to_string())
    }
}

/// The refs in `refs` that are not candidates.
fn unknown<'a>(state: &TidyState, refs: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    refs.into_iter()
        .filter(|r| state.candidate(r).is_none())
        .cloned()
        .collect()
}

#[tool_router(router = core_tool_router)]
impl TidyServer {
    #[tool(
        name = "propose_tidy",
        description = "Put the tidy list to the user. List the refs to delete and the refs you are \
unsure about; every other candidate is kept. The user approves (Kitty then deletes the chats), \
asks for revisions (you get their words back: revise and call again), or rejects it. Call it once \
the list is ready."
    )]
    pub async fn propose_tidy(&self, Parameters(req): Parameters<ProposeRequest>) -> String {
        let state = match self.state(req.session_id.as_deref()).await {
            Ok(s) => s,
            Err(e) => return refuse(e),
        };
        let bad = unknown(&state, req.delete.iter().chain(&req.unsure));
        if !bad.is_empty() {
            return refuse(format!("not candidates: {}", bad.join(", ")));
        }
        json!({
            "ok": true,
            "note": "The user approved the list. Kitty is deleting the chats, and this chat \
                     with them. Say nothing more."
        })
        .to_string()
    }

    #[tool(
        name = "read_chats",
        description = "Read the first and last exchange of candidate chats, by ref. Only refs the \
user has allowed you to read; ask by listing them as unsure in propose_tidy."
    )]
    pub async fn read_chats(&self, Parameters(req): Parameters<ReadRequest>) -> String {
        let state = match self.state(req.session_id.as_deref()).await {
            Ok(s) => s,
            Err(e) => return refuse(e),
        };
        let bad = unknown(&state, &req.refs);
        if !bad.is_empty() {
            return refuse(format!("not candidates: {}", bad.join(", ")));
        }
        let (allowed, refused): (Vec<&String>, Vec<&String>) =
            req.refs.iter().partition(|r| state.readable.contains(r));
        if allowed.is_empty() {
            return refuse(
                "the user has not allowed you to read these chats. List them as unsure in \
                 propose_tidy; the user can allow reading from there.",
            );
        }
        let mut out = String::new();
        for r in allowed.iter().take(MAX_READ) {
            let Some(c) = state.candidate(r) else {
                continue;
            };
            out.push_str(&format!("## {r}: {}\n", c.title));
            let Some(id) = c.session_ids.first() else {
                continue;
            };
            match first_and_last_exchange(&self.pool, id).await {
                Ok(ex) if ex.is_empty() => out.push_str("(no messages)\n"),
                Ok(ex) => {
                    let one = ex.len() == 1;
                    for (i, e) in ex.iter().enumerate() {
                        let label = match (one, i) {
                            (true, _) => "Only exchange",
                            (false, 0) => "First exchange",
                            _ => "Last exchange",
                        };
                        out.push_str(&format!("{label}\nUser: {}\n", e.user));
                        match &e.assistant {
                            Some(a) => out.push_str(&format!("Assistant: {a}\n")),
                            None => out.push_str("Assistant: (no answer)\n"),
                        }
                    }
                }
                Err(e) => out.push_str(&format!("(could not be read: {e})\n")),
            }
            out.push('\n');
        }
        if allowed.len() > MAX_READ {
            out.push_str(&format!(
                "Read the first {MAX_READ}; call again for the rest.\n"
            ));
        }
        if !refused.is_empty() {
            let refused: Vec<&str> = refused.iter().map(|r| r.as_str()).collect();
            out.push_str(&format!(
                "Not read (the user has not allowed it): {}\n",
                refused.join(", ")
            ));
        }
        out
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for TidyServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            instructions: Some("Propose which of the user's chats to delete.".to_string()),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sessions;
    use crate::tidy::tests::{candidate, message, pool};

    async fn server_with_chats() -> TidyServer {
        let pool = pool().await;
        for id in ["t", "a", "b"] {
            sessions::create_session_for_app(&pool, id, id, "kitty")
                .await
                .unwrap();
        }
        message(&pool, "a", "user", "what is a b550m board").await;
        message(&pool, "a", "assistant", "a motherboard").await;
        message(&pool, "b", "user", "secret").await;
        crate::tidy::set_candidates(
            &pool,
            "t",
            vec![candidate("c1", &["a"]), candidate("c2", &["b"])],
        )
        .await
        .unwrap();
        TidyServer::new(pool)
    }

    fn read(refs: &[&str], session: Option<&str>) -> Parameters<ReadRequest> {
        Parameters(ReadRequest {
            refs: refs.iter().map(|r| r.to_string()).collect(),
            session_id: session.map(str::to_string),
        })
    }

    #[tokio::test]
    async fn nothing_is_read_without_the_users_consent() {
        let server = server_with_chats().await;
        let out = server.read_chats(read(&["c1"], Some("t"))).await;
        assert!(out.contains("not allowed"), "{out}");
        assert!(!out.contains("b550m"));

        crate::tidy::allow_reading(&server.pool, "t", &["c1".into()])
            .await
            .unwrap();
        let out = server.read_chats(read(&["c1", "c2"], Some("t"))).await;
        assert!(
            out.contains("b550m") && out.contains("a motherboard"),
            "{out}"
        );
        assert!(!out.contains("secret"), "c2 was not allowed: {out}");
        assert!(out.contains("Not read") && out.contains("c2"), "{out}");
    }

    #[tokio::test]
    async fn only_a_tidy_chat_may_use_the_tools() {
        let server = server_with_chats().await;
        let out = server.read_chats(read(&["c1"], Some("a"))).await;
        assert!(out.contains("not tidying"), "{out}");
        let out = server.read_chats(read(&["c1"], None)).await;
        assert!(out.contains("not tidying"), "{out}");
    }

    #[tokio::test]
    async fn a_proposal_names_only_candidates() {
        let server = server_with_chats().await;
        let propose = |delete: &[&str]| {
            Parameters(ProposeRequest {
                delete: delete.iter().map(|r| r.to_string()).collect(),
                unsure: vec![],
                reasons: BTreeMap::new(),
                summary: None,
                session_id: Some("t".into()),
            })
        };
        assert!(server
            .propose_tidy(propose(&["c1"]))
            .await
            .contains("\"ok\":true"));
        let out = server.propose_tidy(propose(&["c7"])).await;
        assert!(out.contains("not candidates: c7"), "{out}");
    }
}
