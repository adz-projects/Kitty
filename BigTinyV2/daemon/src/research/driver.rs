//! Runs a research run from an approved plan to a written deliverable.
//!
//! A supervisor task watches for runs that are ready to move (the plan was
//! approved; the daemon restarted mid-run) and drives each in its own task,
//! one step at a time, persisting after every step so a restart resumes at
//! the next step boundary rather than repeating work. Each step starts from a
//! fresh context: a wave's planner gets a new child session and a projection
//! of the ledger, never an earlier step's transcript.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashSet;
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tokio::sync::Notify;

use crate::agent::orchestrator::{DelegateOutcome, DelegateRun, Orchestrator, SpawnRefusal};
use crate::agent::Agent;
use crate::models::specialist::Specialist;
use crate::provider::queue::Priority;
use crate::research::contract::{waves_for, Contract, OutputFormat, Section};
use crate::research::ledger::{Body, DeadEnd, Defect, Finding, Ledger, Question, Task, TaskKind};
use crate::research::ops::{self, META_ROLE, META_RUN, META_WAVE, ROLE_PLANNER};
use crate::research::{projection, prompts, report, server};
use crate::server::events::{SSEEvent, SSEEventType};
use crate::storage::research::{self as store, RunRow};
use crate::storage::{messages, sessions, specialists};

/// How often the supervisor looks for work without being woken.
const SWEEP_EVERY: Duration = Duration::from_secs(30);
/// A plan not submitted this long after its turn ended has been abandoned
/// (rejected, or the model never called the tool).
const PLAN_GRACE: Duration = Duration::from_secs(20);
/// Most unplaced findings offered to each section writer.
const MAX_UNPLACED_PER_SECTION: usize = 30;
/// Most evidence lines in one writer prompt.
const MAX_EVIDENCE_PER_SECTION: usize = 80;
/// Fewest new usable findings a wave must add to count as productive.
const SATURATION_FINDINGS: usize = 2;
/// Planner session step ceiling.
const PLANNER_MAX_STEPS: i64 = 30;

static WAKE: Lazy<Notify> = Lazy::new(Notify::new);
static ACTIVE: Lazy<DashSet<String>> = Lazy::new(DashSet::new);

/// Ask the supervisor to look for work now (a plan was just approved).
pub fn wake() {
    WAKE.notify_one();
}

pub struct Driver {
    pub pool: SqlitePool,
    pub agent: Arc<Agent>,
    pub orchestrator: Arc<Orchestrator>,
}

/// Start the supervisor. Runs for the life of the daemon.
pub fn spawn_supervisor(driver: Arc<Driver>) {
    tokio::spawn(async move {
        // A run still planning when the daemon starts lost its turn 0 with the
        // old process: nothing can approve it any more.
        if let Ok(runs) = store::list_live_runs(&driver.pool).await {
            for run in runs.iter().filter(|r| r.status == store::STATUS_PLANNING) {
                let _ = store::set_status(
                    &driver.pool,
                    &run.id,
                    store::STATUS_CANCELLED,
                    Some("The engine restarted before the plan was approved."),
                )
                .await;
                driver.publish(run, None).await;
            }
        }
        loop {
            driver.sweep().await;
            tokio::select! {
                _ = WAKE.notified() => {}
                _ = tokio::time::sleep(SWEEP_EVERY) => {}
            }
        }
    });
}

impl Driver {
    async fn sweep(self: &Arc<Self>) {
        let runs = match store::list_live_runs(&self.pool).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("research: could not list runs: {e}");
                return;
            }
        };
        let busy: BTreeSet<String> = self.agent.active_session_ids().into_iter().collect();
        for run in runs {
            if run.status == store::STATUS_PLANNING {
                self.reap_abandoned_plan(&run, &busy).await;
                continue;
            }
            if !ACTIVE.insert(run.id.clone()) {
                continue;
            }
            let me = self.clone();
            tokio::spawn(async move {
                let id = run.id.clone();
                me.drive(run).await;
                ACTIVE.remove(&id);
            });
        }
    }

    /// Cancel a run whose turn 0 ended without an approved plan.
    async fn reap_abandoned_plan(&self, run: &RunRow, busy: &BTreeSet<String>) {
        let Some(chat) = run.session_id.as_deref() else {
            return;
        };
        if busy.contains(chat) {
            return;
        }
        let age = chrono::NaiveDateTime::parse_from_str(&run.updated_at, "%Y-%m-%d %H:%M:%S")
            .ok()
            .map(|t| chrono::Utc::now().naive_utc() - t)
            .and_then(|d| d.to_std().ok())
            .unwrap_or_default();
        if age < PLAN_GRACE {
            return;
        }
        let _ = store::set_status(
            &self.pool,
            &run.id,
            store::STATUS_CANCELLED,
            Some("The plan was not approved."),
        )
        .await;
        clear_chat_role(&self.pool, chat).await;
        self.publish(run, None).await;
    }

    /// Drive one run to its end in this task, as the supervisor would. For
    /// tests and tools; the daemon itself goes through the supervisor.
    pub async fn drive_now(self: &Arc<Self>, run_id: &str) -> Result<(), String> {
        let run = store::get_run(&self.pool, run_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("no such run")?;
        self.drive(run).await;
        Ok(())
    }

    async fn drive(self: &Arc<Self>, run: RunRow) {
        if let Some(chat) = run.session_id.as_deref() {
            clear_chat_role(&self.pool, chat).await;
        }
        let id = run.id.clone();
        if let Err(e) = self.drive_inner(&id).await {
            tracing::warn!(run = %id, "research run failed: {e}");
            if store::set_status(&self.pool, &id, store::STATUS_FAILED, Some(&e))
                .await
                .unwrap_or(false)
            {
                if let Ok(Some(run)) = store::get_run(&self.pool, &id).await {
                    self.publish(&run, None).await;
                }
            }
        }
    }

    async fn drive_inner(self: &Arc<Self>, id: &str) -> Result<(), String> {
        loop {
            let run = store::get_run(&self.pool, id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("the run disappeared")?;
            match run.status.as_str() {
                store::STATUS_RESEARCHING => {
                    let waves = waves_for(run.max_turns);
                    if run.turns_used >= waves {
                        self.advance(&run, store::STATUS_CRITIQUING).await?;
                        continue;
                    }
                    let wave = run.turns_used + 1;
                    let productive = self.run_wave(&run, wave).await?;
                    store::add_turn(&self.pool, id, 0)
                        .await
                        .map_err(|e| e.to_string())?;
                    if !productive && wave >= 2 && wave < waves {
                        // Saturated: skip the remaining waves, keep the budget
                        // for the critique and the write-up.
                        for skipped in (wave + 1)..=waves {
                            let _ = store::start_step(&self.pool, id, skipped, "wave", None).await;
                            let _ = store::finish_step(
                                &self.pool,
                                id,
                                skipped,
                                "skipped",
                                Some("Skipped: the last wave found little that was new."),
                            )
                            .await;
                            store::add_turn(&self.pool, id, 0)
                                .await
                                .map_err(|e| e.to_string())?;
                        }
                    }
                }
                store::STATUS_CRITIQUING => {
                    self.run_critic(&run).await?;
                    store::add_turn(&self.pool, id, 0)
                        .await
                        .map_err(|e| e.to_string())?;
                    self.advance(&run, store::STATUS_WRITING).await?;
                }
                store::STATUS_WRITING => {
                    self.run_write(&run).await?;
                    store::add_turn(&self.pool, id, 0)
                        .await
                        .map_err(|e| e.to_string())?;
                    self.advance(&run, store::STATUS_DONE).await?;
                    return Ok(());
                }
                // Cancelled (or ended some other way) between steps.
                _ => return Ok(()),
            }
        }
    }

    async fn advance(&self, run: &RunRow, to: &str) -> Result<(), String> {
        if store::set_status(&self.pool, &run.id, to, None)
            .await
            .map_err(|e| e.to_string())?
        {
            if let Ok(Some(run)) = store::get_run(&self.pool, &run.id).await {
                self.publish(&run, None).await;
            }
        }
        Ok(())
    }

    async fn still_live(&self, id: &str) -> bool {
        matches!(store::get_run(&self.pool, id).await, Ok(Some(r)) if r.is_live())
    }

    // ------------------------------------------------------------ waves

    /// One wave. Returns whether it was productive.
    async fn run_wave(self: &Arc<Self>, run: &RunRow, wave: i64) -> Result<bool, String> {
        let loaded = ops::load(&self.pool, &run.id).await?;
        let contract = loaded.contract.ok_or("the run has no approved plan")?;
        let before: BTreeSet<i64> = usable_findings(&loaded.ledger);
        let chat = run.session_id.clone().ok_or("the run's chat was deleted")?;

        let planner = self.planner_session(run, &chat, wave, &contract).await?;
        store::start_step(&self.pool, &run.id, wave, "wave", Some(&planner))
            .await
            .map_err(|e| e.to_string())?;
        self.publish(run, Some(json!({"step": "planning", "wave": wave})))
            .await;

        let prompt = prompts::planner_prompt(
            &projection::render(&projection::ProjectionInput {
                brief: &run.brief,
                contract: &contract,
                ledger: &loaded.ledger,
                wave,
                max_turns: run.max_turns,
                max_chars: projection::DEFAULT_MAX_CHARS,
            }),
            wave,
            run.max_turns,
        );
        let mut tasks = self.planner_turn(&planner, &prompt, &run.id, wave).await;
        if tasks.is_empty() && self.still_live(&run.id).await {
            tasks = self
                .planner_turn(
                    &planner,
                    &format!(
                        "You ended your turn without setting the frontier. Follow the \
                         instructions above: record what is needed, then call \
                         `{}`. Its error message, if any, says exactly what is missing.",
                        server::FRONTIER_TOOL
                    ),
                    &run.id,
                    wave,
                )
                .await;
        }
        if tasks.is_empty() {
            tasks = self.fallback_frontier(run, &contract, wave).await?;
        }
        if !self.still_live(&run.id).await {
            return Ok(true);
        }

        self.publish(
            run,
            Some(json!({"step": "researching", "wave": wave, "tasks": tasks.len()})),
        )
        .await;
        let tokens = self.dispatch(run, &chat, &contract, wave, &tasks).await?;
        store::add_tokens(&self.pool, &run.id, tokens)
            .await
            .map_err(|e| e.to_string())?;

        let after = ops::load_ledger(&self.pool, &run.id).await?;
        let new: Vec<i64> = usable_findings(&after)
            .difference(&before)
            .copied()
            .collect();
        let summary = format!(
            "{} tasks, {} new findings, {} sources in all",
            tasks.len(),
            new.len(),
            after.of_kind("source").count()
        );
        store::finish_step(&self.pool, &run.id, wave, "succeeded", Some(&summary))
            .await
            .map_err(|e| e.to_string())?;
        let open_p1 = after.of_kind("question").any(|e| {
            matches!(&e.body, Body::Question(q) if q.priority == 1)
                && matches!(after.status(e.seq), crate::research::ledger::Status::Open)
        });
        Ok(new.len() >= SATURATION_FINDINGS || open_p1)
    }

    /// A fresh session for a wave's planner: the chat's model, the ledger
    /// tools and nothing else, no approver.
    async fn planner_session(
        &self,
        run: &RunRow,
        chat: &str,
        wave: i64,
        contract: &Contract,
    ) -> Result<String, String> {
        let id = format!(
            "research_{}_w{wave}_{}",
            run.id.chars().take(8).collect::<String>(),
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        let name = format!(
            "Research wave {wave}: {}",
            crate::research::ledger::clip(&contract.title, 60)
        );
        sessions::create_session_for_app(&self.pool, &id, &name, &run.app_id)
            .await
            .map_err(|e| e.to_string())?;
        let parent_meta = chat_metadata(&self.pool, chat).await;
        sessions::update_metadata_with(&self.pool, &id, |mut m| {
            let obj = m.as_object_mut().expect("metadata is an object");
            for key in [
                "provider",
                "model",
                "cwd",
                "chat_dir",
                "working_dirs",
                "thinking_effort",
            ] {
                if let Some(v) = parent_meta.get(key) {
                    obj.insert(key.into(), v.clone());
                }
            }
            obj.insert("mode".into(), json!("chat"));
            obj.insert(
                "tool_allow".into(),
                json!([
                    server::VIEW_TOOL,
                    server::RECORD_TOOL,
                    server::FRONTIER_TOOL
                ]),
            );
            obj.insert("hitl_policy".into(), json!("auto_reject"));
            obj.insert("max_steps".into(), json!(PLANNER_MAX_STEPS));
            obj.insert(META_RUN.into(), json!(run.id));
            obj.insert(META_ROLE.into(), json!(ROLE_PLANNER));
            obj.insert(META_WAVE.into(), json!(wave));
            m
        })
        .await
        .map_err(|e| e.to_string())?;
        let _ = sessions::set_parent(&self.pool, &id, chat, &run.app_id).await;
        Ok(id)
    }

    /// Run the planner and return the tasks it set for this wave.
    async fn planner_turn(
        &self,
        session: &str,
        prompt: &str,
        run_id: &str,
        wave: i64,
    ) -> Vec<Task> {
        if let Err(e) = self
            .agent
            .run_turn_and_wait_outcome(session, prompt, Priority::Background)
            .await
        {
            tracing::warn!(session, "research planner turn failed: {e}");
        }
        wave_tasks(&self.pool, run_id, wave).await
    }

    /// A frontier built from the ledger when the planner set none, so a wave
    /// is never simply lost: the most important open questions and leads,
    /// plus one tangent.
    async fn fallback_frontier(
        &self,
        run: &RunRow,
        contract: &Contract,
        wave: i64,
    ) -> Result<Vec<Task>, String> {
        let ledger = ops::load_ledger(&self.pool, &run.id).await?;
        let mut open: Vec<(u8, i64, String)> = ledger
            .entries()
            .filter(|e| matches!(ledger.status(e.seq), crate::research::ledger::Status::Open))
            .filter_map(|e| match &e.body {
                Body::Question(q) => Some((q.priority, e.seq, q.text.clone())),
                Body::Lead(l) if !l.tangent => Some((l.priority, e.seq, l.text.clone())),
                _ => None,
            })
            .collect();
        open.sort();
        let mut objectives: Vec<(Option<i64>, String)> = open
            .into_iter()
            .take(3)
            .map(|(_, s, t)| (Some(s), t))
            .collect();
        for q in &contract.questions {
            if objectives.len() >= 3 {
                break;
            }
            objectives.push((None, q.clone()));
        }
        let mut tasks: Vec<(Task, Option<i64>)> = objectives
            .into_iter()
            .map(|(seq, objective)| {
                (
                    Task {
                        kind: TaskKind::Search,
                        objective,
                        output_format: "findings with evidence and source URLs".into(),
                        guidance: "Prefer primary and authoritative sources.".into(),
                        boundaries: contract.boundaries.join("; "),
                        refs: vec![],
                        tangent: false,
                    },
                    seq,
                )
            })
            .collect();
        let tangent = ledger
            .of_kind("lead")
            .filter(|e| matches!(&e.body, Body::Lead(l) if l.tangent))
            .find(|e| matches!(ledger.status(e.seq), crate::research::ledger::Status::Open))
            .and_then(|e| match &e.body {
                Body::Lead(l) => Some((l.text.clone(), e.seq)),
                _ => None,
            });
        let (objective, seq) = match tangent {
            Some((t, s)) => (t, Some(s)),
            None => (
                format!(
                    "Look for something unexpected but relevant around: {}. Related work, people, \
                     events or ideas the plan did not anticipate.",
                    contract.objective
                ),
                None,
            ),
        };
        tasks.push((
            Task {
                kind: TaskKind::Search,
                objective,
                output_format: "findings with evidence and source URLs".into(),
                guidance: "Follow what is surprising.".into(),
                boundaries: contract.boundaries.join("; "),
                refs: vec![],
                tangent: true,
            },
            seq,
        ));
        let mut out = Vec::new();
        for (task, pursues) in tasks {
            let a = ops::append(&self.pool, &run.id, wave, &Body::Task(task.clone())).await?;
            if let Some(p) = pursues {
                let _ =
                    ops::link(&self.pool, &run.id, a.seq(), p, "pursues", "fallback", wave).await;
            }
            out.push(task);
        }
        Ok(out)
    }

    /// Run the frontier's tasks in parallel and write what they found into
    /// the ledger. Returns the tokens spent.
    async fn dispatch(
        &self,
        run: &RunRow,
        chat: &str,
        contract: &Contract,
        wave: i64,
        tasks: &[Task],
    ) -> Result<i64, String> {
        let ledger = ops::load_ledger(&self.pool, &run.id).await?;
        // The task entries this wave, in order, to tie findings to what their
        // task pursued.
        let task_seqs: Vec<i64> = ledger
            .of_kind("task")
            .filter(|e| e.wave == wave)
            .map(|e| e.seq)
            .collect();
        let mut runs: Vec<(DelegateRun, usize, Option<String>)> = Vec::new();
        for (i, task) in tasks.iter().enumerate() {
            let name = match task.kind {
                TaskKind::Search => "researcher",
                TaskKind::Read => "summarizer",
                TaskKind::Compute => "analyst",
            };
            let spec = self.specialist(&run.app_id, name).await?;
            let prompt = prompts::task_prompt(
                &task.objective,
                &task.output_format,
                &task.guidance,
                &task.boundaries,
                &run.brief,
            );
            let base = delegate(
                &spec,
                chat,
                &delegate_instructions(contract, task.kind, prompt),
            );
            if task.kind == TaskKind::Read && !task.refs.is_empty() {
                for r in &task.refs {
                    let mut d = base.clone();
                    d.prompt = format!("{}\n\nWork from this source: {r}", d.prompt);
                    runs.push((d, i, Some(r.clone())));
                }
            } else {
                let mut d = base;
                if !task.refs.is_empty() {
                    d.prompt = format!(
                        "{}\n\nStart from these sources:\n{}",
                        d.prompt,
                        task.refs
                            .iter()
                            .map(|r| format!("- {r}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                }
                runs.push((d, i, None));
            }
        }
        let cap = self.orchestrator.completable_fan_out().max(1);
        runs.truncate(cap);
        let outcomes = self
            .orchestrator
            .run_many(runs.iter().map(|(d, _, _)| d.clone()).collect())
            .await;
        let mut tokens = 0;
        for ((_, i, from_ref), outcome) in runs.into_iter().zip(outcomes) {
            let task = &tasks[i];
            let task_seq = task_seqs.get(i).copied();
            match outcome_answer(outcome) {
                Ok(out) => {
                    tokens += out.host.tokens;
                    let pursued = match task_seq {
                        Some(s) => ledger.pursued_by(s),
                        None => vec![],
                    };
                    self.absorb(
                        run,
                        contract,
                        wave,
                        task,
                        &pursued,
                        from_ref.as_deref(),
                        &out.answer,
                    )
                    .await?;
                }
                Err(why) => {
                    ops::append(
                        &self.pool,
                        &run.id,
                        wave,
                        &Body::DeadEnd(DeadEnd {
                            query: task.objective.clone(),
                            note: format!("The delegate did not finish: {why}"),
                        }),
                    )
                    .await?;
                }
            }
        }
        Ok(tokens)
    }

    /// Write one delegate's report into the ledger.
    #[allow(clippy::too_many_arguments)]
    async fn absorb(
        &self,
        run: &RunRow,
        contract: &Contract,
        wave: i64,
        task: &Task,
        pursued: &[i64],
        from_ref: Option<&str>,
        answer: &str,
    ) -> Result<(), String> {
        let Ok(parsed) = serde_json::from_str::<DelegateReport>(answer.trim()) else {
            ops::append(
                &self.pool,
                &run.id,
                wave,
                &Body::DeadEnd(DeadEnd {
                    query: task.objective.clone(),
                    note: "The delegate's report could not be read.".into(),
                }),
            )
            .await?;
            return Ok(());
        };
        let mut recorded = 0;
        for f in parsed.findings {
            if f.claim.trim().is_empty() {
                continue;
            }
            let url = if f.source_url.trim().is_empty() {
                from_ref.map(str::to_string)
            } else {
                Some(f.source_url.clone())
            };
            let finding = Finding {
                claim: f.claim,
                evidence: f.evidence,
                source_url: url,
                attribution: f.attribution,
                tangent: task.tangent,
            };
            let rec = ops::record_finding(
                &self.pool,
                &run.id,
                Some(contract),
                wave,
                finding,
                &f.source_title,
                &f.cells,
                &f.sections,
            )
            .await?;
            recorded += 1;
            if f.answers_objective {
                for p in pursued {
                    let _ = ops::link(
                        &self.pool,
                        &run.id,
                        rec.finding.seq(),
                        *p,
                        "answers",
                        "",
                        wave,
                    )
                    .await;
                }
            }
        }
        for q in parsed.open_questions {
            if !q.trim().is_empty() {
                ops::append(
                    &self.pool,
                    &run.id,
                    wave,
                    &Body::Question(Question {
                        text: q,
                        priority: 2,
                    }),
                )
                .await?;
            }
        }
        for d in parsed.dead_ends {
            if !d.trim().is_empty() {
                ops::append(
                    &self.pool,
                    &run.id,
                    wave,
                    &Body::DeadEnd(DeadEnd {
                        query: d,
                        note: String::new(),
                    }),
                )
                .await?;
            }
        }
        if recorded == 0 {
            let note = [parsed.notes, parsed.refusals.join("; ")]
                .into_iter()
                .filter(|s| !s.trim().is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            ops::append(
                &self.pool,
                &run.id,
                wave,
                &Body::DeadEnd(DeadEnd {
                    query: task.objective.clone(),
                    note: if note.is_empty() {
                        "Nothing found.".into()
                    } else {
                        note
                    },
                }),
            )
            .await?;
        }
        Ok(())
    }

    /// The app's own definition of `name` if it has an enabled one, else the
    /// built-in — even one the app retired from ordinary chats, which is how
    /// research may use `analyst` while chats may not.
    async fn specialist(&self, app_id: &str, name: &str) -> Result<Specialist, String> {
        if let Ok(Some(s)) = specialists::resolve(&self.pool, app_id, name).await {
            if s.enabled {
                return Ok(s);
            }
        }
        if let Ok(Some(s)) = specialists::get_any(&self.pool, &format!("builtin:{name}")).await {
            return Ok(s);
        }
        // Not seeded (a fresh database): the definition the seed would write.
        crate::specialists::registry::all()
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| format!("the {name} specialist is not installed"))
    }

    // ------------------------------------------------------------ critic

    async fn run_critic(self: &Arc<Self>, run: &RunRow) -> Result<(), String> {
        let turn = run.turns_used + 1;
        let chat = run.session_id.clone().ok_or("the run's chat was deleted")?;
        let loaded = ops::load(&self.pool, &run.id).await?;
        let contract = loaded.contract.ok_or("the run has no approved plan")?;
        store::start_step(&self.pool, &run.id, turn, "critic", None)
            .await
            .map_err(|e| e.to_string())?;
        self.publish(run, Some(json!({"step": "critiquing"}))).await;

        let prompt = format!(
            "{}\n\n{}",
            prompts::contract_brief(&contract),
            digest(&contract, &loaded.ledger)
        );
        let base = DelegateRun {
            name: "research_critic".into(),
            parent_session_id: chat.clone(),
            prompt,
            system_prompt: Some(prompts::CRITIC_PROMPT.into()),
            provider: None,
            model: None,
            tool_allow: vec![
                "lean_web_scrape".into(),
                "lean_doc_search".into(),
                "lean_doc_read_chunk".into(),
            ],
            response_schema: Some(prompts::critic_schema()),
            max_steps: 15,
            reasoning_cap: None,
            max_concurrent: None,
            force_host: false,
        };
        // A second opinion from a different model, when the app has one.
        let mut critics = vec![base.clone()];
        if let Some(other) = self.other_host(&run.app_id, &chat).await {
            critics.push(DelegateRun {
                provider: Some(other),
                ..base
            });
        }
        let outcomes = self.orchestrator.run_many(critics).await;
        let mut kept: BTreeSet<i64> = BTreeSet::new();
        let mut gaps: Vec<(i64, String)> = Vec::new();
        let mut tokens = 0;
        let mut defects = 0;
        for outcome in outcomes {
            let Ok(out) = outcome_answer(outcome) else {
                continue;
            };
            tokens += out.host.tokens;
            let Ok(parsed) = serde_json::from_str::<CriticReport>(out.answer.trim()) else {
                continue;
            };
            for t in parsed.kept_tangents {
                kept.extend(report::cited(&format!("[{t}]")));
            }
            for d in parsed.defects {
                let query = d.suggested_query.trim().to_string();
                let a = ops::append(
                    &self.pool,
                    &run.id,
                    turn,
                    &Body::Defect(Defect {
                        kind: d.kind.clone(),
                        target: d.target,
                        description: d.description,
                        suggested_query: (!query.is_empty()).then(|| query.clone()),
                    }),
                )
                .await?;
                defects += 1;
                if d.kind == "gap" && !query.is_empty() {
                    gaps.push((a.seq(), query));
                }
            }
        }
        let outline = json!({"unexpected": kept.iter().collect::<Vec<_>>()});
        store::set_outline(&self.pool, &run.id, &outline.to_string())
            .await
            .map_err(|e| e.to_string())?;

        // Fill the gaps the critic named, once, in parallel.
        let cap = self.orchestrator.completable_fan_out().max(1);
        gaps.truncate(cap);
        if !gaps.is_empty() && self.still_live(&run.id).await {
            self.publish(
                run,
                Some(json!({"step": "filling_gaps", "gaps": gaps.len()})),
            )
            .await;
            let spec = self.specialist(&run.app_id, "researcher").await?;
            let runs: Vec<DelegateRun> = gaps
                .iter()
                .map(|(_, q)| {
                    let prompt = prompts::task_prompt(
                        q,
                        "findings with evidence and source URLs",
                        "Prefer primary and authoritative sources.",
                        &contract.boundaries.join("; "),
                        &run.brief,
                    );
                    delegate(
                        &spec,
                        &chat,
                        &delegate_instructions(&contract, TaskKind::Search, prompt),
                    )
                })
                .collect();
            let task = Task {
                kind: TaskKind::Search,
                objective: String::new(),
                output_format: String::new(),
                guidance: String::new(),
                boundaries: String::new(),
                refs: vec![],
                tangent: false,
            };
            let outcomes = self.orchestrator.run_many(runs).await;
            for ((defect, query), outcome) in gaps.into_iter().zip(outcomes) {
                let Ok(out) = outcome_answer(outcome) else {
                    continue;
                };
                tokens += out.host.tokens;
                let before = usable_findings(&ops::load_ledger(&self.pool, &run.id).await?);
                let t = Task {
                    objective: query,
                    ..task.clone()
                };
                self.absorb(run, &contract, turn, &t, &[], None, &out.answer)
                    .await?;
                let after = usable_findings(&ops::load_ledger(&self.pool, &run.id).await?);
                for f in after.difference(&before) {
                    let _ = ops::link(&self.pool, &run.id, *f, defect, "addresses", "", turn).await;
                }
            }
        }
        store::add_tokens(&self.pool, &run.id, tokens)
            .await
            .map_err(|e| e.to_string())?;
        store::finish_step(
            &self.pool,
            &run.id,
            turn,
            "succeeded",
            Some(&format!("{defects} defects, {} tangents kept", kept.len())),
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// A healthy, tool-capable provider other than the chat's own, for a
    /// second critic: the most capable one the user allows for delegates.
    async fn other_host(&self, app_id: &str, chat: &str) -> Option<String> {
        let parent = chat_metadata(&self.pool, chat)
            .await
            .get("provider")
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut candidates: Vec<_> = self
            .agent
            .router()
            .subagent_candidates(app_id, parent.as_deref())
            .into_iter()
            .filter(|c| {
                c.healthy
                    && c.supports_tools
                    && !c.is_parent_provider
                    && c.subagent_role.as_deref() != Some("never")
            })
            .collect();
        candidates.sort_by_key(|c| std::cmp::Reverse(c.capability_rank.unwrap_or(0)));
        candidates.into_iter().next().map(|c| c.provider_id)
    }

    // ------------------------------------------------------------ write

    async fn run_write(self: &Arc<Self>, run: &RunRow) -> Result<(), String> {
        let turn = run.turns_used + 1;
        let chat = run.session_id.clone().ok_or("the run's chat was deleted")?;
        let loaded = ops::load(&self.pool, &run.id).await?;
        let contract = loaded.contract.ok_or("the run has no approved plan")?;
        let ledger = loaded.ledger;
        store::start_step(&self.pool, &run.id, turn, "write", None)
            .await
            .map_err(|e| e.to_string())?;
        self.publish(run, Some(json!({"step": "writing"}))).await;

        let unexpected: Vec<i64> = run
            .outline
            .as_deref()
            .and_then(|o| serde_json::from_str::<Value>(o).ok())
            .and_then(|o| o.get("unexpected").cloned())
            .and_then(|u| serde_json::from_value::<Vec<i64>>(u).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|s| ledger.is_usable_finding(*s))
            .collect();
        let mut sections: Vec<(Section, Vec<i64>)> = contract
            .sections
            .iter()
            .map(|s| (s.clone(), ledger.findings_in_section(&s.id)))
            .collect();
        if !unexpected.is_empty() {
            sections.push((
                Section {
                    id: "unexpected".into(),
                    title: "Unexpected connections".into(),
                    description: "What the research turned up beyond the plan that bears on \
                                  the question."
                        .into(),
                },
                unexpected,
            ));
        }
        let unplaced: Vec<i64> = ledger
            .of_kind("finding")
            .filter(|e| {
                ledger.is_usable_finding(e.seq) && ledger.sections_of(e.seq).next().is_none()
            })
            .map(|e| e.seq)
            .take(MAX_UNPLACED_PER_SECTION)
            .collect();

        let brief = prompts::contract_brief(&contract);
        let writer_run = |prompt: String| DelegateRun {
            name: "research_writer".into(),
            parent_session_id: chat.clone(),
            prompt,
            system_prompt: Some(prompts::WRITER_PROMPT.into()),
            provider: None,
            model: None,
            tool_allow: vec![],
            response_schema: Some(prompts::writer_schema()),
            max_steps: 3,
            reasoning_cap: None,
            max_concurrent: None,
            force_host: false,
        };
        let mut texts: Vec<String> = vec![String::new(); sections.len()];
        let mut prompts_by_section: Vec<Option<String>> = vec![None; sections.len()];
        let mut writes = Vec::new();
        for (i, (section, findings)) in sections.iter().enumerate() {
            if findings.is_empty() && unplaced.is_empty() {
                texts[i] = "_No findings were established for this section._".into();
                continue;
            }
            let prompt = format!(
                "{brief}\nSection: {} — {}\n\nEvidence:\n{}{}",
                section.title,
                section.description,
                evidence_block(&ledger, findings),
                if unplaced.is_empty() || section.id == "unexpected" {
                    String::new()
                } else {
                    format!(
                        "\n\nOther findings, not yet placed (use only those that belong in this \
                         section):\n{}",
                        evidence_block(&ledger, &unplaced)
                    )
                }
            );
            prompts_by_section[i] = Some(prompt.clone());
            writes.push((i, writer_run(prompt)));
        }
        let mut tokens = 0;
        let outcomes = self
            .orchestrator
            .run_many(writes.iter().map(|(_, d)| d.clone()).collect())
            .await;
        for ((i, _), outcome) in writes.iter().zip(outcomes) {
            texts[*i] = match outcome_answer(outcome) {
                Ok(out) => {
                    tokens += out.host.tokens;
                    markdown_field(&out.answer)
                }
                Err(why) => format!("_This section could not be written: {why}_"),
            };
        }

        // Verify: mechanically, then against the cited evidence.
        self.publish(run, Some(json!({"step": "verifying"}))).await;
        let checkable: Vec<usize> = (0..sections.len())
            .filter(|i| prompts_by_section[*i].is_some())
            .collect();
        let verifies: Vec<DelegateRun> = checkable
            .iter()
            .map(|i| DelegateRun {
                name: "research_verifier".into(),
                parent_session_id: chat.clone(),
                prompt: format!(
                    "Section text:\n\n{}\n\nCited evidence:\n{}",
                    texts[*i],
                    evidence_block(&ledger, &report::check(&texts[*i], &ledger).cited)
                ),
                system_prompt: Some(prompts::VERIFIER_PROMPT.into()),
                provider: None,
                model: None,
                tool_allow: vec![],
                response_schema: Some(prompts::verifier_schema()),
                max_steps: 3,
                reasoning_cap: None,
                max_concurrent: None,
                force_host: false,
            })
            .collect();
        let verdicts = self.orchestrator.run_many(verifies).await;
        let mut flagged: HashMap<usize, Vec<String>> = HashMap::new();
        for (i, outcome) in checkable.iter().zip(verdicts) {
            let mut issues = report::check(&texts[*i], &ledger).as_instructions();
            if let Ok(out) = outcome_answer(outcome) {
                tokens += out.host.tokens;
                if let Ok(v) = serde_json::from_str::<VerifierReport>(out.answer.trim()) {
                    for s in v.verdicts.into_iter().filter(|s| s.verdict != "supported") {
                        flagged.entry(*i).or_default().push(s.sentence.clone());
                        issues.push(format!("{}: \"{}\" — {}", s.verdict, s.sentence, s.note));
                    }
                }
            }
            if !issues.is_empty() {
                flagged.entry(*i).or_default();
                let prompt = format!(
                    "{}\n\nYour previous draft:\n\n{}\n\nFix these problems, changing nothing \
                     else:\n- {}",
                    prompts_by_section[*i].as_deref().unwrap_or_default(),
                    texts[*i],
                    issues.join("\n- ")
                );
                prompts_by_section[*i] = Some(prompt);
            } else {
                prompts_by_section[*i] = None;
            }
        }
        let rewrites: Vec<(usize, DelegateRun)> = checkable
            .iter()
            .filter(|i| flagged.contains_key(i))
            .filter_map(|i| prompts_by_section[*i].clone().map(|p| (*i, writer_run(p))))
            .collect();
        let outcomes = self
            .orchestrator
            .run_many(rewrites.iter().map(|(_, d)| d.clone()).collect())
            .await;
        for ((i, _), outcome) in rewrites.iter().zip(outcomes) {
            if let Ok(out) = outcome_answer(outcome) {
                tokens += out.host.tokens;
                let text = markdown_field(&out.answer);
                if !text.trim().is_empty() {
                    texts[*i] = text;
                }
            }
        }
        // What is still unsupported after the rewrite is cut, or kept and
        // marked when the plan allows it.
        let keep = contract.deliverable.allow_unverified;
        for (i, sentences) in &flagged {
            let mut text = texts[*i].clone();
            for s in sentences
                .iter()
                .chain(report::check(&text, &ledger).uncited.iter())
            {
                text = report::settle_sentence(&text, s, keep);
            }
            texts[*i] = text;
        }

        // The opening, from the finished sections.
        let body: String = sections
            .iter()
            .zip(&texts)
            .map(|((s, _), t)| format!("## {}\n\n{}\n", s.title, t.trim()))
            .collect::<Vec<_>>()
            .join("\n");
        let intro_runs = vec![DelegateRun {
            name: "research_writer".into(),
            parent_session_id: chat.clone(),
            prompt: format!("{brief}\n\n{body}"),
            system_prompt: Some(prompts::INTRO_PROMPT.into()),
            provider: chat_metadata(&self.pool, &chat)
                .await
                .get("provider")
                .and_then(Value::as_str)
                .map(str::to_string),
            model: None,
            tool_allow: vec![],
            response_schema: Some(prompts::intro_schema()),
            max_steps: 3,
            reasoning_cap: None,
            max_concurrent: None,
            force_host: false,
        }];
        let intro = match self
            .orchestrator
            .run_many(intro_runs)
            .await
            .pop()
            .map(outcome_answer)
        {
            Some(Ok(out)) => {
                tokens += out.host.tokens;
                markdown_field(&out.answer)
            }
            _ => String::new(),
        };
        store::add_tokens(&self.pool, &run.id, tokens)
            .await
            .map_err(|e| e.to_string())?;

        let (body, sources) = report::number_sources(&body, &ledger);
        let run_now = store::get_run(&self.pool, &run.id).await.ok().flatten();
        let waves_run = run_now
            .as_ref()
            .map(|r| r.turns_used)
            .unwrap_or(turn)
            .min(waves_for(run.max_turns));
        let mut doc = format!("# {}\n\n", contract.title);
        if !intro.trim().is_empty() {
            doc.push_str(intro.trim());
            doc.push_str("\n\n");
        }
        doc.push_str(&body);
        if !sources.is_empty() {
            doc.push_str("\n## Sources\n\n");
            for s in &sources {
                doc.push_str(&format!(
                    "{}. [{}]({})\n",
                    s.number,
                    s.title.replace(['[', ']'], ""),
                    s.url
                ));
            }
        }
        doc.push_str(&format!(
            "\n---\n_Deep research: {waves_run} research wave(s), a critique and a verified \
             write-up; {} findings from {} sources._\n",
            usable_findings(&ledger).len(),
            ledger.of_kind("source").count()
        ));

        // Written beside the chat, in its outputs folder.
        let mut message = doc.clone();
        if let Some(dir) = outputs_dir(&chat_metadata(&self.pool, &chat).await) {
            match write_output(&dir, &contract.title, &doc) {
                Ok(md_path) => {
                    let md = md_path.to_string_lossy().to_string();
                    let _ = store::set_output_path(&self.pool, &run.id, &md).await;
                    let mut saved = format!("Saved to `{md}`");
                    if contract.deliverable.format == OutputFormat::Docx {
                        let docx = md_path.with_extension("docx").to_string_lossy().to_string();
                        let result = self
                            .agent
                            .mcp()
                            .execute_tool_for_app(
                                &run.app_id,
                                "lean_word_write_doc",
                                &json!({"path": docx, "doc_text": doc, "title": contract.title}),
                                None,
                            )
                            .await;
                        if !result.is_error {
                            saved.push_str(&format!(" and `{docx}`"));
                        }
                    }
                    message = format!("{saved}.\n\n{doc}");
                }
                Err(e) => tracing::warn!(run = %run.id, "research: could not save the report: {e}"),
            }
        }
        append_assistant_message(&self.pool, &chat, &message).await?;
        store::finish_step(
            &self.pool,
            &run.id,
            turn,
            "succeeded",
            Some(&format!(
                "{} sections, {} sources",
                sections.len(),
                sources.len()
            )),
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    // ------------------------------------------------------------ progress

    /// Publish the run's state on the app stream and the chat's.
    pub async fn publish(&self, run: &RunRow, step: Option<Value>) {
        let ev = progress_event(&self.pool, run, step).await;
        self.agent.app_events().publish(&run.app_id, ev.clone());
        if let Some(chat) = run.session_id.as_deref() {
            self.agent.emit_to(chat, ev);
        }
    }
}

/// The progress event for a run.
pub async fn progress_event(pool: &SqlitePool, run: &RunRow, step: Option<Value>) -> SSEEvent {
    let detail = run_detail(pool, run).await;
    let mut detail = detail;
    if let (Some(obj), Some(step)) = (detail.as_object_mut(), step) {
        obj.insert("step".into(), step);
    }
    SSEEvent {
        event_type: SSEEventType::ResearchProgress,
        research_id: Some(run.id.clone()),
        session_id: run.session_id.clone(),
        content: Some(run.status.clone()),
        tool_args: Some(detail),
        ..Default::default()
    }
}

/// A run's state as JSON: what the routes return and the progress event
/// carries.
pub async fn run_detail(pool: &SqlitePool, run: &RunRow) -> Value {
    let ledger = ops::load_ledger(pool, &run.id).await.unwrap_or_default();
    let contract = run
        .contract
        .as_deref()
        .and_then(|c| serde_json::from_str::<Contract>(c).ok());
    let coverage: Vec<Value> = contract
        .as_ref()
        .map(|c| {
            projection::coverage(c, &ledger)
                .into_iter()
                .map(|(id, label, n, strong)| {
                    json!({"id": id, "label": label, "findings": n, "corroborated": strong})
                })
                .collect()
        })
        .unwrap_or_default();
    let steps = store::steps(pool, &run.id).await.unwrap_or_default();
    let open = |kind: &str| {
        ledger
            .of_kind(kind)
            .filter(|e| {
                matches!(
                    ledger.status(e.seq),
                    crate::research::ledger::Status::Open
                        | crate::research::ledger::Status::Pursued
                )
            })
            .count()
    };
    json!({
        "id": run.id,
        "session_id": run.session_id,
        "status": run.status,
        "title": contract.as_ref().map(|c| c.title.clone()),
        "brief": run.brief,
        "max_turns": run.max_turns,
        "turns_used": run.turns_used,
        "waves": waves_for(run.max_turns),
        "tokens_used": run.tokens_used,
        "findings": usable_findings(&ledger).len(),
        "sources": ledger.of_kind("source").count(),
        "open_questions": open("question"),
        "open_leads": open("lead"),
        "tangents_paid_off": ledger.paid_off_tangents().len(),
        "coverage": coverage,
        "steps": steps,
        "output_path": run.output_path,
        "error": run.error,
        "created_at": run.created_at,
        "finished_at": run.finished_at,
    })
}

// ---------------------------------------------------------------- helpers

/// What every research delegate answers in, whatever its kind: findings
/// placed against the plan, plus what it could not establish.
pub fn report_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "claim": {"type": "string", "description": "One self-contained claim."},
                        "evidence": {"type": "string", "description": "What the source says, quoted or closely paraphrased."},
                        "source_url": {"type": "string", "description": "Where you read it. Empty string when working from a given document."},
                        "source_title": {"type": "string", "description": "The source's title. Empty string if unknown."},
                        "attribution": {"type": "string", "description": "Whose assertion it is, when it is not established fact. Empty string otherwise."},
                        "cells": {"type": "array", "items": {"type": "string"}, "description": "Coverage cell ids from the plan this counts toward."},
                        "sections": {"type": "array", "items": {"type": "string"}, "description": "Section ids from the plan it belongs in."},
                        "answers_objective": {"type": "boolean", "description": "True if it directly answers your objective."}
                    },
                    "required": ["claim", "evidence", "source_url", "source_title", "attribution", "cells", "sections", "answers_objective"],
                    "additionalProperties": false
                }
            },
            "open_questions": {"type": "array", "items": {"type": "string"}, "description": "What you could not establish. Empty array if none."},
            "dead_ends": {"type": "array", "items": {"type": "string"}, "description": "Searches or sources that gave nothing. Empty array if none."},
            "notes": {"type": "string", "description": "Anything else. Empty string if none."},
            "refusals": {"type": "array", "items": {"type": "string"}, "description": "Tool calls that were refused. Empty array if none."}
        },
        "required": ["findings", "open_questions", "dead_ends", "notes", "refusals"],
        "additionalProperties": false
    })
}

#[derive(Debug, Deserialize)]
struct ReportFinding {
    claim: String,
    #[serde(default)]
    evidence: String,
    #[serde(default)]
    source_url: String,
    #[serde(default)]
    source_title: String,
    #[serde(default)]
    attribution: String,
    #[serde(default)]
    cells: Vec<String>,
    #[serde(default)]
    sections: Vec<String>,
    #[serde(default)]
    answers_objective: bool,
}

#[derive(Debug, Deserialize)]
struct DelegateReport {
    #[serde(default)]
    findings: Vec<ReportFinding>,
    #[serde(default)]
    open_questions: Vec<String>,
    #[serde(default)]
    dead_ends: Vec<String>,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    refusals: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CriticDefect {
    kind: String,
    target: String,
    description: String,
    #[serde(default)]
    suggested_query: String,
}

#[derive(Debug, Deserialize)]
struct CriticReport {
    #[serde(default)]
    defects: Vec<CriticDefect>,
    #[serde(default)]
    kept_tangents: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Verdict {
    sentence: String,
    verdict: String,
    #[serde(default)]
    note: String,
}

#[derive(Debug, Deserialize)]
struct VerifierReport {
    #[serde(default)]
    verdicts: Vec<Verdict>,
}

/// What a delegate of `kind` is told about the plan, on top of its task.
fn delegate_instructions(contract: &Contract, kind: TaskKind, task: String) -> String {
    let cells: Vec<String> = contract
        .coverage
        .iter()
        .map(|c| format!("`{}` ({})", c.id, c.label))
        .collect();
    let sections: Vec<String> = contract
        .sections
        .iter()
        .map(|s| format!("`{}` ({})", s.id, s.title))
        .collect();
    let how = match kind {
        TaskKind::Search => "",
        TaskKind::Read => {
            "\nWork from the document you were given. Report its substance as findings, each \
             anchored in `evidence` to where it says so (page or section)."
        }
        TaskKind::Compute => {
            "\nReport each figure as a finding (`claim`: the figure and what it measures; \
             `evidence`: how it was computed)."
        }
    };
    format!(
        "{task}{how}\n\nPlace each finding: `cells` from {}; `sections` from {}.",
        cells.join(", "),
        sections.join(", ")
    )
}

/// A delegate run from a specialist definition and a prompt, answering in the
/// research report shape.
fn delegate(spec: &Specialist, chat: &str, prompt: &str) -> DelegateRun {
    DelegateRun {
        name: spec.name.clone(),
        parent_session_id: chat.to_string(),
        prompt: prompt.to_string(),
        system_prompt: Some(spec.system_prompt.clone()),
        provider: spec.provider.clone(),
        model: spec.model.clone(),
        tool_allow: spec
            .tool_allow
            .iter()
            .filter(|t| {
                !crate::specialists::server::SESSION_SCOPED_TOOLS.contains(&t.as_str())
                    && !server::RESEARCH_TOOLS.contains(&t.as_str())
            })
            .cloned()
            .collect(),
        response_schema: Some(report_schema()),
        max_steps: spec.max_steps,
        reasoning_cap: spec.reasoning_cap,
        max_concurrent: spec.max_concurrent,
        force_host: false,
    }
}

fn outcome_answer(
    outcome: Result<Result<DelegateOutcome, String>, SpawnRefusal>,
) -> Result<DelegateOutcome, String> {
    match outcome {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(why)) => Err(why),
        Err(refusal) => Err(refusal.to_string()),
    }
}

/// The `markdown` field of a writer's answer, or the answer itself when it
/// came back as prose.
fn markdown_field(answer: &str) -> String {
    serde_json::from_str::<Value>(answer.trim())
        .ok()
        .and_then(|v| {
            v.get("markdown")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| answer.trim().to_string())
}

fn usable_findings(ledger: &Ledger) -> BTreeSet<i64> {
    ledger
        .of_kind("finding")
        .filter(|e| ledger.is_usable_finding(e.seq))
        .map(|e| e.seq)
        .collect()
}

/// Evidence lines for a writer, verifier or critic.
fn evidence_block(ledger: &Ledger, findings: &[i64]) -> String {
    let disputes = ledger.disputes();
    findings
        .iter()
        .take(MAX_EVIDENCE_PER_SECTION)
        .filter_map(|seq| {
            let Body::Finding(f) = &ledger.get(*seq)?.body else {
                return None;
            };
            let domain = f
                .source_url
                .as_deref()
                .and_then(crate::research::ledger::domain_of)
                .unwrap_or_else(|| "document".into());
            let others: Vec<String> = disputes
                .iter()
                .filter_map(|(a, b)| {
                    if a == seq {
                        Some(format!("L{b}"))
                    } else if b == seq {
                        Some(format!("L{a}"))
                    } else {
                        None
                    }
                })
                .collect();
            let disputed = if others.is_empty() {
                String::new()
            } else {
                format!(" [disputed by {}]", others.join(", "))
            };
            let who = if f.attribution.is_empty() {
                String::new()
            } else {
                format!(" (per {})", f.attribution)
            };
            Some(format!(
                "- L{seq}: {}{who} — evidence: \"{}\" (source: {domain}; {} independent){disputed}",
                f.claim,
                crate::research::ledger::clip(&f.evidence, 400),
                ledger.independent_sources(*seq)
            ))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The ledger by section, for the critic.
fn digest(contract: &Contract, ledger: &Ledger) -> String {
    let mut out = String::from("# The research, by section\n");
    let line = |seq: i64| -> Option<String> {
        let e = ledger.get(seq)?;
        let tangent = matches!(&e.body, Body::Finding(f) if f.tangent);
        Some(format!(
            "- L{seq}{}: {} ({} independent sources){}",
            if tangent { " (tangent)" } else { "" },
            e.body.headline(),
            ledger.independent_sources(seq),
            if matches!(
                ledger.status(seq),
                crate::research::ledger::Status::Disputed
            ) {
                " [disputed]"
            } else {
                ""
            }
        ))
    };
    for s in &contract.sections {
        out.push_str(&format!("\n## {} (`{}`)\n", s.title, s.id));
        let found = ledger.findings_in_section(&s.id);
        if found.is_empty() {
            out.push_str("- nothing yet\n");
        }
        for seq in found.into_iter().take(MAX_EVIDENCE_PER_SECTION) {
            if let Some(l) = line(seq) {
                out.push_str(&l);
                out.push('\n');
            }
        }
    }
    let unplaced: Vec<String> = ledger
        .of_kind("finding")
        .filter(|e| ledger.is_usable_finding(e.seq) && ledger.sections_of(e.seq).next().is_none())
        .filter_map(|e| line(e.seq))
        .take(MAX_EVIDENCE_PER_SECTION)
        .collect();
    if !unplaced.is_empty() {
        out.push_str("\n## Not placed in any section\n");
        out.push_str(&unplaced.join("\n"));
        out.push('\n');
    }
    out.push_str("\n## Coverage\n");
    for (id, label, n, strong) in projection::coverage(contract, ledger) {
        out.push_str(&format!(
            "- `{id}` ({label}): {n} findings, {strong} corroborated\n"
        ));
    }
    out
}

/// The tasks a wave's frontier set, in order.
async fn wave_tasks(pool: &SqlitePool, run_id: &str, wave: i64) -> Vec<Task> {
    ops::load_ledger(pool, run_id)
        .await
        .map(|l| {
            l.of_kind("task")
                .filter(|e| e.wave == wave)
                .filter_map(|e| match &e.body {
                    Body::Task(t) => Some(t.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn chat_metadata(pool: &SqlitePool, session: &str) -> Value {
    sessions::get_session_metadata(pool, session)
        .await
        .ok()
        .flatten()
        .and_then(|m| serde_json::from_str(&m).ok())
        .unwrap_or_else(|| json!({}))
}

/// Remove turn 0's role from the chat, so its later turns are ordinary.
pub async fn clear_chat_role(pool: &SqlitePool, chat: &str) {
    let _ = sessions::update_metadata_with(pool, chat, |mut m| {
        if let Some(obj) = m.as_object_mut() {
            obj.remove(META_ROLE);
            obj.remove(META_RUN);
            obj.remove(META_WAVE);
        }
        m
    })
    .await;
}

/// The chat's outputs folder, from its metadata.
fn outputs_dir(meta: &Value) -> Option<PathBuf> {
    let dir = meta
        .get("chat_dir")
        .or_else(|| meta.get("cwd"))
        .and_then(Value::as_str)
        .filter(|d| !d.trim().is_empty())?;
    Some(Path::new(dir).join("outputs"))
}

/// Save the report as `<dir>/<slug>.md`, never overwriting an earlier one.
fn write_output(dir: &Path, title: &str, doc: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let slug = report::file_slug(title);
    let mut path = dir.join(format!("{slug}.md"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{slug}-{n}.md"));
        n += 1;
    }
    std::fs::write(&path, doc)?;
    Ok(path)
}

async fn append_assistant_message(pool: &SqlitePool, chat: &str, text: &str) -> Result<(), String> {
    let row = messages::MessageRow {
        rowid: 0,
        id: uuid::Uuid::new_v4().to_string(),
        session_id: chat.to_string(),
        role: "assistant".into(),
        content: Some(text.to_string()),
        tool_calls: None,
        tool_call_id: None,
        token_count: None,
        content_format: None,
        created_at: Some(chrono::Utc::now()),
        reasoning: None,
        provider_id: None,
        model: None,
        memory_block: None,
    };
    messages::save_messages(pool, chat, &[row])
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_schema_is_strict_mode_clean() {
        let s = report_schema();
        let item = &s["properties"]["findings"]["items"];
        assert_eq!(
            item["properties"].as_object().unwrap().len(),
            item["required"].as_array().unwrap().len()
        );
        assert_eq!(
            s["properties"].as_object().unwrap().len(),
            s["required"].as_array().unwrap().len()
        );
    }

    #[test]
    fn a_writer_answer_is_read_as_json_or_prose() {
        assert_eq!(
            markdown_field(r#"{"markdown":"Hi [L1].","notes":""}"#),
            "Hi [L1]."
        );
        assert_eq!(markdown_field("plain prose"), "plain prose");
    }

    #[test]
    fn outputs_go_beside_the_chat_and_never_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let meta = json!({"chat_dir": dir.path().to_string_lossy()});
        let out = outputs_dir(&meta).unwrap();
        let a = write_output(&out, "My Report", "one").unwrap();
        let b = write_output(&out, "My Report", "two").unwrap();
        assert!(a.ends_with("outputs/my-report.md") || a.ends_with("outputs\\my-report.md"));
        assert_ne!(a, b);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "one");
        assert!(outputs_dir(&json!({})).is_none());
    }
}
