//! The in-process MCP server through which a research run's models touch the
//! ledger: `submit_research_plan` (turn 0), and the wave planner's
//! `research_view`, `research_record` and `research_set_frontier`.
//!
//! Like `specialists`, the connection is daemon-lifetime and shared by every
//! session, so the calling session is injected into each call's arguments by
//! `agent::loop_` ([`RESEARCH_TOOLS`]) and the run, role and wave are read from
//! that session's metadata. A model can neither name another run nor act
//! outside its role. The tools are also withheld from every session that has
//! no research role (`agent::loop_::withhold_research_tools`).

use std::collections::BTreeSet;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;

use crate::agent::orchestrator::Orchestrator;
use crate::research::contract::Contract;
use crate::research::ledger::{
    Body, DeadEnd, Entity, Lead, Ledger, Question, Status, Task, TaskKind,
};
use crate::research::ops::{self, SessionRole, ROLE_PLAN, ROLE_PLANNER};
use crate::research::projection;
use crate::storage::research as store;

pub const SUBMIT_PLAN_TOOL: &str = "submit_research_plan";
pub const VIEW_TOOL: &str = "research_view";
pub const RECORD_TOOL: &str = "research_record";
pub const FRONTIER_TOOL: &str = "research_set_frontier";

/// Every tool this server owns. All need the calling session injected.
pub const RESEARCH_TOOLS: [&str; 4] = [SUBMIT_PLAN_TOOL, VIEW_TOOL, RECORD_TOOL, FRONTIER_TOOL];

/// The tools a session in `role` is offered. Everything else of this server's
/// is withheld from it.
pub fn tools_for_role(role: Option<&str>) -> &'static [&'static str] {
    match role {
        Some(ROLE_PLAN) => &[SUBMIT_PLAN_TOOL],
        Some(ROLE_PLANNER) => &[VIEW_TOOL, RECORD_TOOL, FRONTIER_TOOL],
        _ => &[],
    }
}

/// Fewest frontier items a wave may set (or the cap, when lower).
const MIN_FRONTIER: usize = 3;
/// Frontier ceiling when no orchestrator is attached (tests).
const DEFAULT_FRONTIER_CAP: usize = 9;

/// Link kinds a planner may write. `cites`, `pursues` and `addresses` are
/// written by the engine alongside the entries they belong to.
const PLANNER_LINKS: [&str; 5] = [
    "answers",
    "duplicates",
    "disputes",
    "supersedes",
    "derived_from",
];

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SubmitPlanRequest {
    /// The research plan.
    pub plan: Contract,
    /// Host-injected, never model-supplied.
    #[serde(default)]
    #[schemars(skip)]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ViewRequest {
    /// "projection" (default): the research so far, as you were first shown
    /// it. "entries": ledger entries in full, filtered by the fields below.
    #[serde(default)]
    pub what: Option<String>,
    /// For "entries": only this kind (finding, source, question, lead,
    /// dead_end, entity, defect, task).
    #[serde(default)]
    pub kind: Option<String>,
    /// For "entries": only these entries (the numbers in `L12`).
    #[serde(default)]
    pub seqs: Option<Vec<i64>>,
    /// For "entries": only entries from this wave.
    #[serde(default)]
    pub wave: Option<i64>,
    #[serde(default)]
    #[schemars(skip)]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LeadIn {
    /// What to look into next.
    pub text: String,
    /// What in the findings suggests it.
    pub why: String,
    /// True for a productive tangent: outside the plan's coverage, but it
    /// could change or enrich the answer.
    #[serde(default)]
    pub tangent: bool,
    /// 1 (most important) to 3.
    #[serde(default)]
    pub priority: Option<u8>,
    /// The entries you read it out of (numbers from `L12`).
    #[serde(default)]
    pub derived_from: Vec<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct QuestionIn {
    pub text: String,
    /// 1 (most important) to 3.
    #[serde(default)]
    pub priority: Option<u8>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LinkIn {
    pub from: i64,
    pub to: i64,
    /// answers (finding -> question/lead), duplicates (entry -> the one it
    /// repeats), disputes (finding -> finding), supersedes (newer -> older
    /// finding), derived_from (lead -> finding).
    pub kind: String,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PlacementIn {
    /// The entry to place (the number in `L12`).
    pub seq: i64,
    /// Coverage cell ids from the plan.
    #[serde(default)]
    pub cells: Vec<String>,
    /// Section ids from the plan.
    #[serde(default)]
    pub sections: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecordRequest {
    #[serde(default)]
    pub leads: Vec<LeadIn>,
    #[serde(default)]
    pub questions: Vec<QuestionIn>,
    #[serde(default)]
    pub entities: Vec<Entity>,
    #[serde(default)]
    pub dead_ends: Vec<DeadEnd>,
    #[serde(default)]
    pub links: Vec<LinkIn>,
    #[serde(default)]
    pub placements: Vec<PlacementIn>,
    #[serde(default)]
    #[schemars(skip)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TaskIn {
    #[serde(flatten)]
    pub task: Task,
    /// The leads or questions this task acts on (numbers from `L12`).
    #[serde(default)]
    pub pursues: Vec<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FrontierRequest {
    pub tasks: Vec<TaskIn>,
    #[serde(default)]
    #[schemars(skip)]
    pub session_id: Option<String>,
}

/// Why a frontier is refused, if it is. Pure, so the rules are testable
/// without a server: the frontier has to be the product of reading the
/// findings, and it always has to leave room for the unexpected.
pub fn check_frontier(
    ledger: &Ledger,
    wave: i64,
    tasks: &[TaskIn],
    cap: usize,
) -> Result<(), String> {
    let mut problems: Vec<String> = Vec::new();
    let min = MIN_FRONTIER.min(cap);
    if tasks.len() < min || tasks.len() > cap {
        problems.push(format!(
            "set {min} to {cap} tasks (you set {})",
            tasks.len()
        ));
    }
    if ledger.of_kind("task").any(|e| e.wave == wave) {
        problems.push("this wave's frontier is already set".into());
    }
    if !tasks.iter().any(|t| t.task.tangent) {
        problems.push(
            "include at least one tangent task: something the findings (or, in the first wave, \
             the topic) suggest outside the plan's coverage that could change or enrich the answer"
                .into(),
        );
    }
    for (i, t) in tasks.iter().enumerate() {
        if t.task.objective.trim().is_empty() {
            problems.push(format!("task {} has no objective", i + 1));
        }
        if t.task.kind == TaskKind::Read && t.task.refs.is_empty() {
            problems.push(format!("task {} is a read task with no refs", i + 1));
        }
        for p in &t.pursues {
            if !matches!(
                ledger.get(*p).map(|e| &e.body),
                Some(Body::Lead(_)) | Some(Body::Question(_))
            ) {
                problems.push(format!(
                    "task {} pursues L{p}, which is not a lead or question",
                    i + 1
                ));
            }
        }
    }

    // From the moment there are findings to read, every wave must read them:
    // new leads, drawn from what was found, with a tangent among them.
    let has_findings = ledger.of_kind("finding").next().is_some();
    if has_findings {
        let new_leads: Vec<_> = ledger.of_kind("lead").filter(|e| e.wave == wave).collect();
        if new_leads.is_empty() {
            problems.push(format!(
                "record this wave's leads first ({RECORD_TOOL}): next steps you read out of the \
                 findings, each with `derived_from`"
            ));
        } else if !new_leads
            .iter()
            .any(|e| matches!(&e.body, Body::Lead(l) if l.tangent))
        {
            problems.push(format!(
                "at least one of this wave's leads must be a productive tangent (`tangent: true`); \
                 record one with {RECORD_TOOL}"
            ));
        }
    }
    let leads: BTreeSet<i64> = ledger
        .of_kind("lead")
        .filter(|e| matches!(ledger.status(e.seq), Status::Open | Status::Pursued))
        .map(|e| e.seq)
        .collect();
    if !leads.is_empty() {
        let on_leads = tasks
            .iter()
            .filter(|t| t.pursues.iter().any(|p| leads.contains(p)))
            .count();
        let needed = tasks.len().div_ceil(2);
        if on_leads < needed {
            problems.push(format!(
                "at least {needed} tasks must pursue an open lead (list it in `pursues`); \
                 {on_leads} do"
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!("Frontier not set: {}.", problems.join("; ")))
    }
}

#[derive(Clone)]
pub struct ResearchServer {
    pool: SqlitePool,
    orchestrator: Option<Arc<Orchestrator>>,
    tool_router: ToolRouter<Self>,
}

fn refuse(message: impl Into<String>) -> String {
    json!({"ok": false, "error": message.into()}).to_string()
}

impl ResearchServer {
    pub fn new(pool: SqlitePool, orchestrator: Option<Arc<Orchestrator>>) -> Self {
        Self {
            pool,
            orchestrator,
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

    /// The calling session's role, if it is `want`.
    async fn role(&self, session_id: Option<&str>, want: &str) -> Result<SessionRole, String> {
        let session_id = session_id
            .filter(|s| !s.is_empty())
            .ok_or("this session is not part of a research run")?;
        let role = ops::session_role(&self.pool, session_id)
            .await
            .ok_or("this session is not part of a research run")?;
        if role.role != want {
            return Err(format!("this tool is not for the {} step", role.role));
        }
        Ok(role)
    }

    fn frontier_cap(&self) -> usize {
        self.orchestrator
            .as_ref()
            .map(|o| o.completable_fan_out())
            .unwrap_or(DEFAULT_FRONTIER_CAP)
            .max(1)
    }
}

#[tool_router(router = core_tool_router)]
impl ResearchServer {
    #[tool(
        name = "submit_research_plan",
        description = "Submit the research plan for the user's approval. The user approves it, \
asks for revisions (you get their words back: revise and submit again), or rejects it. Call it \
once you have a complete plan; do not research the topic itself first."
    )]
    pub async fn submit_research_plan(
        &self,
        Parameters(req): Parameters<SubmitPlanRequest>,
    ) -> String {
        let role = match self.role(req.session_id.as_deref(), ROLE_PLAN).await {
            Ok(r) => r,
            Err(e) => return refuse(e),
        };
        let mut plan = req.plan;
        if let Err(why) = plan.validate() {
            return refuse(why);
        }
        let Ok(contract) = serde_json::to_string(&plan) else {
            return refuse("the plan could not be stored");
        };
        if let Err(e) = store::set_contract(&self.pool, &role.run_id, &contract, plan.turns).await {
            return refuse(format!("the plan could not be stored: {e}"));
        }
        match store::set_status(&self.pool, &role.run_id, store::STATUS_RESEARCHING, None).await {
            Ok(true) => {
                crate::research::driver::wake();
                json!({
                "ok": true,
                "note": "The plan is approved and the research now runs in the background. Tell \
                         the user in one sentence that it has started, then end your turn. Do \
                         not research anything yourself."
                })
                .to_string()
            }
            Ok(false) => refuse("this research run has already ended"),
            Err(e) => refuse(format!("the run could not be started: {e}")),
        }
    }

    #[tool(
        name = "research_view",
        description = "Read the research ledger. With no arguments: the research so far, as you \
were first shown it. what=\"entries\" with kind, seqs or wave: those entries in full."
    )]
    pub async fn research_view(&self, Parameters(req): Parameters<ViewRequest>) -> String {
        let role = match self.role(req.session_id.as_deref(), ROLE_PLANNER).await {
            Ok(r) => r,
            Err(e) => return refuse(e),
        };
        let loaded = match ops::load(&self.pool, &role.run_id).await {
            Ok(l) => l,
            Err(e) => return refuse(e),
        };
        if req.what.as_deref().unwrap_or("projection") == "projection" {
            let Some(contract) = loaded.contract.as_ref() else {
                return refuse("the run has no approved plan");
            };
            return projection::render(&projection::ProjectionInput {
                brief: &loaded.run.brief,
                contract,
                ledger: &loaded.ledger,
                wave: role.wave,
                max_turns: loaded.run.max_turns,
                max_chars: projection::DEFAULT_MAX_CHARS,
            });
        }
        let seqs: Option<BTreeSet<i64>> = req.seqs.map(|s| s.into_iter().collect());
        let mut out = Vec::new();
        for e in loaded.ledger.entries() {
            if req.kind.as_deref().is_some_and(|k| k != e.body.kind())
                || req.wave.is_some_and(|w| w != e.wave)
                || seqs.as_ref().is_some_and(|s| !s.contains(&e.seq))
            {
                continue;
            }
            out.push(json!({
                "id": format!("L{}", e.seq),
                "wave": e.wave,
                "kind": e.body.kind(),
                "status": format!("{:?}", loaded.ledger.status(e.seq)).to_lowercase(),
                "cells": loaded.ledger.cells_of(e.seq).collect::<Vec<_>>(),
                "sections": loaded.ledger.sections_of(e.seq).collect::<Vec<_>>(),
                "entry": serde_json::from_str::<serde_json::Value>(&e.body.payload_json())
                    .unwrap_or_default(),
            }));
            if out.len() >= 200 {
                break;
            }
        }
        json!({"ok": true, "entries": out}).to_string()
    }

    #[tool(
        name = "research_record",
        description = "Write to the research ledger: leads (next steps read out of the findings, \
each with derived_from; mark productive tangents), questions, entities, dead ends, links between \
entries (answers, duplicates, disputes, supersedes, derived_from) and placements of entries in \
the plan's coverage cells and sections. Entries are never edited; what became of one is a link."
    )]
    pub async fn research_record(&self, Parameters(req): Parameters<RecordRequest>) -> String {
        let role = match self.role(req.session_id.as_deref(), ROLE_PLANNER).await {
            Ok(r) => r,
            Err(e) => return refuse(e),
        };
        let loaded = match ops::load(&self.pool, &role.run_id).await {
            Ok(l) => l,
            Err(e) => return refuse(e),
        };
        let Some(contract) = loaded.contract.as_ref() else {
            return refuse("the run has no approved plan");
        };
        let (pool, run, wave) = (&self.pool, role.run_id.as_str(), role.wave);
        let has_findings = loaded.ledger.of_kind("finding").next().is_some();
        let mut written = Vec::new();
        let mut rejected = Vec::new();

        for lead in req.leads {
            let sources: Vec<i64> = lead
                .derived_from
                .iter()
                .copied()
                .filter(|s| loaded.ledger.get(*s).is_some())
                .collect();
            if has_findings && sources.is_empty() {
                rejected.push(format!(
                    "lead {:?}: give `derived_from`, the entries it was read out of",
                    lead.text
                ));
                continue;
            }
            let body = Body::Lead(Lead {
                text: lead.text,
                why: lead.why,
                tangent: lead.tangent,
                priority: lead.priority.unwrap_or(2).clamp(1, 3),
            });
            match ops::append(pool, run, wave, &body).await {
                Ok(a) => {
                    for s in sources {
                        let _ = ops::link(pool, run, a.seq(), s, "derived_from", "", wave).await;
                    }
                    written.push(format!("L{} lead", a.seq()));
                }
                Err(e) => rejected.push(e),
            }
        }
        let simple: Vec<Body> = req
            .questions
            .into_iter()
            .map(|q| {
                Body::Question(Question {
                    text: q.text,
                    priority: q.priority.unwrap_or(2).clamp(1, 3),
                })
            })
            .chain(req.entities.into_iter().map(Body::Entity))
            .chain(req.dead_ends.into_iter().map(Body::DeadEnd))
            .collect();
        for body in simple {
            match ops::append(pool, run, wave, &body).await {
                Ok(store::Appended::New(s)) => written.push(format!("L{s} {}", body.kind())),
                Ok(store::Appended::Existing(s)) => {
                    written.push(format!("L{s} {} (already recorded)", body.kind()))
                }
                Err(e) => rejected.push(e),
            }
        }
        for l in req.links {
            if !PLANNER_LINKS.contains(&l.kind.as_str()) {
                rejected.push(format!("link kind {:?} is not one you can write", l.kind));
                continue;
            }
            match ops::link(pool, run, l.from, l.to, &l.kind, &l.reason, wave).await {
                Ok(true) => written.push(format!("L{} {} L{}", l.from, l.kind, l.to)),
                Ok(false) => rejected.push(format!(
                    "L{} {} L{}: both must be existing entries (or it was already recorded)",
                    l.from, l.kind, l.to
                )),
                Err(e) => rejected.push(e),
            }
        }
        for p in req.placements {
            if loaded.ledger.get(p.seq).is_none() {
                rejected.push(format!("L{} does not exist", p.seq));
                continue;
            }
            match ops::place(pool, run, contract, p.seq, &p.cells, &p.sections, wave).await {
                Ok(unknown) if unknown.is_empty() => written.push(format!("L{} placed", p.seq)),
                Ok(unknown) => rejected.push(format!(
                    "L{}: no cell or section {:?} in the plan",
                    p.seq, unknown
                )),
                Err(e) => rejected.push(e),
            }
        }
        json!({"ok": rejected.is_empty(), "written": written, "rejected": rejected}).to_string()
    }

    #[tool(
        name = "research_set_frontier",
        description = "Set this wave's frontier: the tasks delegates will research in parallel \
when your turn ends. Each task is a full brief (objective, output_format, guidance, boundaries) \
of kind search (the web), read (documents in refs) or compute (figures). Include at least one \
tangent task. Once leads exist, at least half the tasks must pursue one (`pursues`). Once there \
are findings, record this wave's leads first, a tangent among them. Call once, last."
    )]
    pub async fn research_set_frontier(
        &self,
        Parameters(req): Parameters<FrontierRequest>,
    ) -> String {
        let role = match self.role(req.session_id.as_deref(), ROLE_PLANNER).await {
            Ok(r) => r,
            Err(e) => return refuse(e),
        };
        let ledger = match ops::load_ledger(&self.pool, &role.run_id).await {
            Ok(l) => l,
            Err(e) => return refuse(e),
        };
        if let Err(why) = check_frontier(&ledger, role.wave, &req.tasks, self.frontier_cap()) {
            return refuse(why);
        }
        let (pool, run, wave) = (&self.pool, role.run_id.as_str(), role.wave);
        let mut ids = Vec::new();
        for t in req.tasks {
            let seq = match ops::append(pool, run, wave, &Body::Task(t.task)).await {
                Ok(a) => a.seq(),
                Err(e) => return refuse(e),
            };
            for p in t.pursues {
                let _ = ops::link(pool, run, seq, p, "pursues", "", wave).await;
            }
            ids.push(format!("L{seq}"));
        }
        json!({
            "ok": true,
            "tasks": ids,
            "note": "Frontier set. The tasks run when your turn ends: end it now with one \
                     sentence on what this wave is after."
        })
        .to_string()
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ResearchServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            instructions: Some("Read and write a deep research run's ledger.".to_string()),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::research::ledger::tests::{finding, lead, link, row};

    fn task(tangent: bool, pursues: Vec<i64>) -> TaskIn {
        TaskIn {
            task: Task {
                kind: TaskKind::Search,
                objective: "find".into(),
                output_format: "findings".into(),
                guidance: "web".into(),
                boundaries: "1940-70".into(),
                refs: vec![],
                tangent,
            },
            pursues,
        }
    }

    #[test]
    fn every_tool_is_registered_and_scoped() {
        let names: Vec<String> = ResearchServer::core_tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        for t in RESEARCH_TOOLS {
            assert!(names.contains(&t.to_string()), "{t} missing");
        }
        assert_eq!(tools_for_role(Some(ROLE_PLAN)), &[SUBMIT_PLAN_TOOL]);
        assert!(tools_for_role(None).is_empty());
        assert!(tools_for_role(Some("critic")).is_empty());
    }

    #[test]
    fn the_first_wave_needs_a_tangent_task_but_no_leads() {
        let empty = Ledger::default();
        let plain = vec![
            task(false, vec![]),
            task(false, vec![]),
            task(false, vec![]),
        ];
        let err = check_frontier(&empty, 1, &plain, 9).unwrap_err();
        assert!(err.contains("tangent task"), "{err}");
        let ok = vec![task(false, vec![]), task(false, vec![]), task(true, vec![])];
        assert!(check_frontier(&empty, 1, &ok, 9).is_ok());
        let err = check_frontier(&empty, 1, &ok[..2], 9).unwrap_err();
        assert!(err.contains("set 3 to 9 tasks"), "{err}");
    }

    #[test]
    fn later_waves_must_read_the_findings_and_follow_leads() {
        // Wave 1 found something; wave 2 has written no leads yet.
        let base = vec![row(
            1,
            1,
            &finding("Robbie is from 1940", "https://isfdb.org/1"),
        )];
        let no_leads = Ledger::build(base.clone(), vec![], vec![]);
        let tasks = vec![task(true, vec![]), task(false, vec![]), task(false, vec![])];
        let err = check_frontier(&no_leads, 2, &tasks, 9).unwrap_err();
        assert!(err.contains("record this wave's leads"), "{err}");

        // Leads, but none a tangent.
        let mut rows = base.clone();
        rows.push(row(2, 2, &lead("Read I, Robot", false)));
        let plain_leads = Ledger::build(rows, vec![link(2, 1, "derived_from")], vec![]);
        let err = check_frontier(&plain_leads, 2, &tasks, 9).unwrap_err();
        assert!(err.contains("productive tangent"), "{err}");

        // A tangent lead too; now half the tasks must pursue a lead.
        let mut rows = base;
        rows.push(row(2, 2, &lead("Read I, Robot", false)));
        rows.push(row(3, 2, &lead("Radio adaptations", true)));
        let good = Ledger::build(rows, vec![link(2, 1, "derived_from")], vec![]);
        let err = check_frontier(&good, 2, &tasks, 9).unwrap_err();
        assert!(err.contains("at least 2 tasks must pursue"), "{err}");
        let tasks = vec![
            task(true, vec![3]),
            task(false, vec![2]),
            task(false, vec![]),
        ];
        assert!(check_frontier(&good, 2, &tasks, 9).is_ok());

        // Pursuing something that is not a lead or question is refused.
        let bad = vec![
            task(true, vec![3]),
            task(false, vec![1]),
            task(false, vec![2]),
        ];
        let err = check_frontier(&good, 2, &bad, 9).unwrap_err();
        assert!(err.contains("L1, which is not a lead"), "{err}");
    }

    #[test]
    fn a_frontier_is_set_once_per_wave() {
        let rows = vec![row(1, 1, &Body::Task(task(true, vec![]).task))];
        let l = Ledger::build(rows, vec![], vec![]);
        let tasks = vec![task(true, vec![]), task(false, vec![]), task(false, vec![])];
        let err = check_frontier(&l, 1, &tasks, 9).unwrap_err();
        assert!(err.contains("already set"), "{err}");
    }
}
