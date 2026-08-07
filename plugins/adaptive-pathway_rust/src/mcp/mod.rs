//! In-process MCP server exposing the two write tools the model may choose:
//! `record` and `forget`. Reads are direct in-process calls; these are the
//! writes the model explicitly makes. Mirrors the kitty-tools in-process
//! shape (same rmcp major, `serve_in_process` handed an arbitrary duplex).

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::engine::PathwayEngine;
use crate::store::beliefs::{Layer, Provenance};
use crate::store::suppressions::SuppressReason;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecordRequest {
    /// The observed fact about the user, as a natural-language statement.
    pub observation: String,
    /// Provenance of the observation. Defaults to "inferred_pattern".
    #[serde(default = "default_provenance")]
    pub provenance: String,
    /// Optional domain hint (e.g. "coding", "writing").
    pub domain_hint: Option<String>,
}

fn default_provenance() -> String {
    "inferred_pattern".to_string()
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum ForgetKind {
    #[default]
    Wrong,
    Outdated,
    Private,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ForgetRequest {
    /// What to forget -- either the exact belief statement (preferred) or a
    /// phrase describing it. The model echoes statements, never UUIDs.
    pub what: String,
    /// Why it should be forgotten.
    #[serde(default)]
    pub reason: ForgetKind,
}

#[derive(Clone)]
pub struct PathwayServer {
    engine: Arc<PathwayEngine>,
    tool_router: ToolRouter<Self>,
    session_id: String,
}

impl PathwayServer {
    pub fn new(engine: Arc<PathwayEngine>, session_id: String) -> Self {
        Self {
            tool_router: Self::core_tool_router(),
            engine,
            session_id,
        }
    }

    /// Sorted list of every currently-registered tool name. The MCP surface
    /// is exactly `record` + `forget` (README + Phase-4 acceptance).
    pub fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        names.sort();
        names
    }

    /// Serve the in-process MCP server over an arbitrary duplex stream.
    pub async fn serve_in_process<S>(&self, stream: S) -> Result<(), String>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
    {
        let server = self.clone().serve(stream).await.map_err(|e| e.to_string())?;
        server.waiting().await.map(|_| ()).map_err(|e| e.to_string())
    }
}

#[tool_router(router = core_tool_router)]
impl PathwayServer {
    /// Record a belief about the user from an explicit model-mediated
    /// observation. Failures return a soft message, never an error the model
    /// must reason about.
    #[tool(name = "record", description = "Record a belief about the user based on what they just said or how they behaved, so future turns can adapt. Returns the recorded statement or a soft failure message.")]
    pub async fn record(&self, Parameters(req): Parameters<RecordRequest>) -> String {
        let obs = req.observation.trim().to_string();
        if obs.is_empty() {
            return json!({"status": "ok", "message": "empty_observation"}).to_string();
        }
        if self.engine.is_paused(&self.session_id).await.unwrap_or(false) {
            return json!({"status": "ok", "message": "memory is paused"}).to_string();
        }
        let layer = Layer::Context;
        let provenance = match req.provenance.as_str() {
            "correction" => Provenance::Correction,
            "direct_statement" => Provenance::DirectStatement,
            "controlled_test" => Provenance::ControlledTest,
            "single_observation" => Provenance::SingleObservation,
            _ => Provenance::InferredPattern,
        };
        let embedding = self.engine.embed.embed(&obs).await;
        let res = crate::belief::synthesis::route_observation(
            &self.engine.db,
            &obs,
            &embedding,
            provenance,
            layer,
            req.domain_hint.as_deref(),
            None,
            None,
            Some(self.session_id.clone()),
            chrono::Utc::now(),
        )
        .await;
        match res {
            Ok(()) => json!({"status": "ok", "recorded": obs}).to_string(),
            Err(e) => {
                json!({"status": "ok", "message": format!("could not record: {e}")}).to_string()
            }
        }
    }

    /// Forget a belief. `reason` controls severity: wrong (permanent
    /// suppression + tombstone), outdated (90-day suppression), private (hard
    /// delete). Returns the exact text dropped for the model to echo.
    #[tool(name = "forget", description = "Forget a belief about the user. reason: wrong (permanent), outdated (temporary), private (hard delete). Returns the exact statement dropped so you can echo it back.")]
    pub async fn forget(&self, Parameters(req): Parameters<ForgetRequest>) -> String {
        let what = req.what.trim().to_string();
        if what.is_empty() {
            return json!({"status": "ok", "message": "nothing to forget"}).to_string();
        }
        let reason = match req.reason {
            ForgetKind::Wrong => SuppressReason::Wrong,
            ForgetKind::Outdated => SuppressReason::Outdated,
            ForgetKind::Private => SuppressReason::Private,
        };
        let recall_ids = self
            .engine
            .db
            .get_state(&self.session_id)
            .await
            .ok()
            .flatten()
            .map(|s| s.last_recall_ids)
            .unwrap_or_default();
        let res = self
            .engine
            .db
            .forget_by_text(&what, &recall_ids, reason)
            .await;
        match res {
            Ok(Some(dropped)) => {
                json!({"status": "ok", "dropped": format!("Dropped: '{dropped}'")}).to_string()
            }
            Ok(None) => {
                json!({"status": "ok", "message": "nothing matched; nothing forgotten"}).to_string()
            }
            Err(e) => {
                json!({"status": "ok", "message": format!("could not forget: {e}")}).to_string()
            }
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PathwayServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
}

/// Serve `PathwayServer` over a duplex stream, for `bigtiny_rust`'s
/// in-process transport wiring (mirrors `kitty_tools::serve_in_process`).
pub async fn serve_in_process<S>(server: Arc<PathwayServer>, stream: S) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    server.serve_in_process(stream).await
}
