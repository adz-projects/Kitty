//! Running one turn as a delegate of another.
//!
//! # Why this is not just `run_turn_and_wait`
//!
//! The turn machinery already runs a session to completion with no client
//! attached — recipes and the scheduler have used it since V1. What it does not
//! do is any of the things that make a *delegated* run safe to hand a model:
//! bound how many can be in flight, stop a child spawning its own children,
//! stop a cancelled parent leaving orphans still burning tokens, or tell anyone
//! watching the parent that a child exists.
//!
//! Those are all one concern — the relationship between a parent turn and the
//! turns it causes — so they live in one place rather than being re-derived by
//! each caller that wants a subagent.
//!
//! # Isolation is the point
//!
//! A child gets its own session. Its tool calls, retries and dead ends never
//! enter the parent's transcript; the parent sees only the final answer. That
//! is the entire efficiency argument, and it is also the risk: a summary can
//! lose something the parent needed. The child session is therefore kept and
//! tagged with `parent_session_id`, so the parent (or a person) can read the
//! full transcript back through `GET /api/chat/{id}/history` when the summary
//! was not enough.

use std::sync::{Arc, OnceLock, Weak};

use serde_json::{json, Value};
use sqlx::SqlitePool;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::agent::subagent_pick::{self, Decision};
use crate::provider::queue::Priority;
use crate::server::events::{SSEEvent, SSEEventType};
use crate::storage::{execution, sessions};

use super::Agent;

/// One delegated run, fully specified by its caller.
///
/// Everything a specialist definition can pin is here rather than read from
/// config inside `run`: the orchestrator does not know what a specialist *is*,
/// only how to run one safely. That keeps the definition format (the
/// `specialists` table) free to change without touching this file.
#[derive(Debug, Clone)]
pub struct DelegateRun {
    /// Name of the specialist, for logs and the parent's status events.
    pub name: String,
    /// The session that asked for this run. Its owner owns the child.
    pub parent_session_id: String,
    /// The request, already rendered — the orchestrator does no templating.
    pub prompt: String,
    /// Prepended as the child's persona.
    pub system_prompt: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Exactly the tools the child may call. Enforced twice — the advertised
    /// set is filtered to it and every dispatch is re-checked against it (see
    /// `agent::loop_`).
    pub tool_allow: Vec<String>,
    /// The shape the child's answer must take. `None` means prose, which is
    /// allowed but forfeits the guarantee the parent is usually after.
    pub response_schema: Option<Value>,
    pub max_steps: i64,
    /// How much of this run may go on reasoning. `None` takes the daemon
    /// default, which is why delegates are capped without every definition
    /// having to say so.
    pub reasoning_cap: Option<crate::agent::tokens::ReasoningCap>,
}

/// Where a delegate actually ran, and how many of it there were.
///
/// Reported back to the model in the tool result and to the user in the
/// `subagent_status` frame, because "what did this cost me" should be answerable
/// from the transcript rather than inferred.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HostChoice {
    pub provider: String,
    pub model: String,
    /// 1 for an ordinary call, N for a fan-out.
    pub instances: usize,
    /// Tokens this delegate's own transcript accounts for.
    ///
    /// Reported rather than folded into the parent's per-session stats: the two
    /// are different sessions and summing them there would double-count on any
    /// view that already walks the parent's children. What the caller actually
    /// needs is for a delegated turn to stop *looking free*, and a number in the
    /// tool result does that without corrupting anything.
    pub tokens: i64,
}

/// A finished delegate run.
///
/// `notes` is separate from the `refusals` field inside a specialist's own
/// schema, and the two are not redundant: `refusals` depends on the model
/// complying with a system message, while `notes` is written by the daemon and
/// cannot be talked out of. A degraded host, a dropped pin or a spent reasoning
/// budget belongs in `notes`.
#[derive(Debug, Clone)]
pub struct DelegateOutcome {
    pub answer: String,
    pub host: HostChoice,
    pub notes: Vec<String>,
}

/// Why a delegated run could not be started.
///
/// Distinguished from a run that started and then failed, which comes back as
/// the inner `Err(String)`: a caller may reasonably retry the second and never
/// the first.
#[derive(Debug)]
pub enum SpawnRefusal {
    /// The parent is itself a delegate. See `Orchestrator::DEPTH_LIMIT`.
    TooDeep,
    /// The parent session has no resolvable owner, so the child would have no
    /// tenant — and an ownerless session is unreachable by every scoped
    /// accessor in `storage::sessions`.
    NoOwner,
    /// No provider may host a delegate. Carries the picker's own explanation,
    /// which names the setting responsible — a user who denied their only model
    /// needs that sentence, not "no specialist available".
    NoHost(String),
    Storage(String),
}

impl std::fmt::Display for SpawnRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooDeep => write!(
                f,
                "a specialist cannot call another specialist; do this work directly"
            ),
            Self::NoOwner => write!(f, "the calling session has no owner"),
            Self::NoHost(why) => write!(f, "{why}"),
            Self::Storage(e) => write!(f, "could not create the delegate session: {e}"),
        }
    }
}

/// Bounds and supervises delegated runs.
pub struct Orchestrator {
    /// Weak, because `Agent` owns the `MCPManager` that owns the tool server
    /// that holds this — an `Arc` here would close that cycle and leak the
    /// whole daemon. A dead upgrade means shutdown, which is a clean refusal.
    agent: OnceLock<Weak<Agent>>,
    db: SqlitePool,
    limiter: Arc<Semaphore>,
    limit: usize,
    /// Applied to any delegate whose definition names no cap of its own. Held
    /// here rather than read from config at spawn so the orchestrator stays the
    /// one place that decides what an unspecified run is allowed to spend.
    default_reasoning_fraction: f64,
    /// Models that may never host a delegate. Consulted at final resolution,
    /// including the step that would otherwise fall back to the parent's own
    /// model — see `subagent_pick`.
    model_deny: Vec<String>,
    /// How long one delegate may run before it is cancelled.
    ///
    /// Nothing else bounds it: `run_turn_and_wait` has no timeout, and
    /// `call_specialist` blocks the parent's tool call, so an unbounded delegate
    /// hangs the user's turn while holding one of a small number of permits.
    timeout: std::time::Duration,
    router: OnceLock<Arc<crate::provider::router::ProviderRouter>>,
}

/// Waves of `specialist_timeout_secs` a fan-out may take in total.
const FAN_OUT_DEADLINE_WAVES: u32 = 3;

impl Orchestrator {
    /// Delegates may not delegate.
    ///
    /// Enforced here *and* by never putting the spawn tool in a child's
    /// `tool_allow`. Two mechanisms for one rule because they fail
    /// differently: the allow-list is per-definition and a user editing one
    /// could undo it, while this check is structural and cannot be configured
    /// away. Recursive delegation is not a feature we are missing — it is how
    /// one request becomes an unbounded tree of paid model calls.
    const DEPTH_LIMIT: usize = 1;

    pub fn new(
        db: SqlitePool,
        max_concurrent: usize,
        default_reasoning_fraction: f64,
        model_deny: Vec<String>,
        timeout_secs: u64,
    ) -> Self {
        let limit = max_concurrent.max(1);
        Self {
            agent: OnceLock::new(),
            db,
            limiter: Arc::new(Semaphore::new(limit)),
            limit,
            default_reasoning_fraction: default_reasoning_fraction.clamp(0.0, 1.0),
            model_deny,
            timeout: std::time::Duration::from_secs(timeout_secs.max(1)),
            router: OnceLock::new(),
        }
    }

    /// Close the loop once `Agent` exists. Idempotent; a second call is
    /// ignored rather than panicking, since nothing good comes of a partially
    /// re-pointed orchestrator.
    pub fn attach(&self, agent: &Arc<Agent>) {
        let _ = self.agent.set(Arc::downgrade(agent));
    }

    /// The router, for host selection. Separate from `attach` only because the
    /// router exists before the agent does.
    pub fn attach_router(&self, router: Arc<crate::provider::router::ProviderRouter>) {
        let _ = self.router.set(router);
    }

    /// How many delegates may run at once.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// How many delegates a single fan-out can realistically finish.
    ///
    /// `limit` delegates run at a time and the batch gets
    /// `FAN_OUT_DEADLINE_WAVES` waves of `timeout` before its shared deadline
    /// expires, so this is the honest ceiling — beyond it, the extra sources
    /// are reported as "the batch ran out of time before this source was
    /// reached" no matter how long anyone waits.
    ///
    /// Callers use it to *refuse* an oversized fan-out up front. That matters
    /// because the alternative is not merely slower, it is the failure mode
    /// `MAX_FAN_OUT` already rejects by name: silently doing part of the work.
    /// A cap that only the deadline enforces does exactly that, one element at
    /// a time, with no single message saying the request was too big.
    pub fn completable_fan_out(&self) -> usize {
        self.limit.saturating_mul(FAN_OUT_DEADLINE_WAVES as usize)
    }

    /// Run one delegate to completion and return its final answer.
    ///
    /// The permit is taken *before* the session is created, so a run waiting
    /// its turn leaves no half-built transcript behind if the caller gives up,
    /// and it is held for the whole run including tool execution — the point is
    /// to bound concurrent *agents*, which the provider queue does not do.
    pub async fn run(
        &self,
        spec: DelegateRun,
    ) -> Result<Result<DelegateOutcome, String>, SpawnRefusal> {
        self.run_by(spec, tokio::time::Instant::now() + self.timeout)
            .await
    }

    /// One delegate, bounded by an absolute deadline rather than its own clock.
    ///
    /// A fan-out shares one deadline across every child so the *batch* is
    /// bounded, not just each element. Without that, `specialist_timeout_secs`
    /// bounds a single run and a fan-out quietly re-opens the hole it was added
    /// to close: thirty-two sources against three permits is eleven waves, and
    /// eleven waves of a five-minute ceiling is most of an hour with the
    /// parent's tool call blocked the whole time.
    async fn run_by(
        &self,
        spec: DelegateRun,
        deadline: tokio::time::Instant,
    ) -> Result<Result<DelegateOutcome, String>, SpawnRefusal> {
        let Some(agent) = self.agent.get().and_then(Weak::upgrade) else {
            return Err(SpawnRefusal::Storage("daemon is shutting down".into()));
        };

        let app_id = sessions::owner_of(&self.db, &spec.parent_session_id)
            .await
            .ok()
            .flatten()
            .ok_or(SpawnRefusal::NoOwner)?;

        if Self::DEPTH_LIMIT == 1 {
            match sessions::parent_of(&self.db, &spec.parent_session_id).await {
                Ok(Some(_)) => return Err(SpawnRefusal::TooDeep),
                Ok(None) => {}
                Err(e) => return Err(SpawnRefusal::Storage(e.to_string())),
            }
        }

        // The parent's metadata, read once. Two things need it — which provider
        // the parent is on, and which directories it may reach — and a fan-out
        // multiplies every query here by the number of sources, so reading the
        // same row twice per child is not a rounding error at 32 refs.
        let parent_meta: Value = sessions::get_session(&self.db, &spec.parent_session_id)
            .await
            .ok()
            .flatten()
            .and_then(|r| r.metadata)
            .and_then(|m| serde_json::from_str(&m).ok())
            .unwrap_or_else(|| json!({}));

        // Which provider this runs on, decided before anything is created so a
        // refusal leaves no half-built transcript behind.
        //
        // The parent's own provider is passed in so the picker can tell "the
        // same single-slot llama-server the parent is using" (whose KV cache the
        // delegate would evict on every call) apart from "some other
        // single-slot server" (which costs the parent nothing).
        let parent_provider = parent_meta
            .get("provider")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let mut notes: Vec<String> = Vec::new();
        let (host_provider, host_model) = match self.router.get() {
            Some(router) => {
                let candidates =
                    router.subagent_candidates(&app_id, parent_provider.as_deref());
                let pin = spec
                    .provider
                    .as_deref()
                    .filter(|p| !p.is_empty())
                    .map(|p| (p, spec.model.as_deref().filter(|m| !m.is_empty())));
                match subagent_pick::choose_host(&candidates, pin, &self.model_deny) {
                    Decision::Chosen {
                        provider_id,
                        model,
                        note,
                    } => {
                        if let Some(n) = note {
                            notes.push(n);
                        }
                        (Some(provider_id), model)
                    }
                    Decision::Refused { reason } => return Err(SpawnRefusal::NoHost(reason)),
                }
            }
            // No router attached (tests). Fall back to whatever the definition
            // pinned, which is the pre-selection behaviour.
            None => (spec.provider.clone(), spec.model.clone()),
        };

        let _permit = self.limiter.clone().acquire_owned().await;

        // Checked after the permit, which is where the waiting actually happens.
        // A source the batch never reached is reported as such rather than
        // started and immediately killed — the caller can then re-run just the
        // ones that are missing, which is the whole reason failures here are per
        // element.
        if tokio::time::Instant::now() >= deadline {
            return Ok(Err(
                "the batch ran out of time before this source was reached".to_string()
            ));
        }

        let child_id = Uuid::new_v4().to_string();
        let child_name = format!("{}: {}", spec.name, short(&spec.prompt));
        sessions::create_session_for_app(&self.db, &child_id, &child_name, &app_id)
            .await
            .map_err(|e| SpawnRefusal::Storage(e.to_string()))?;

        // Everything the loop reads per turn, written once. `hitl_policy` and
        // the absent spawn tool are the two entries that make this run safe to
        // leave unattended; the rest is the specialist's own definition.
        let mut meta = json!({
            "mode": "chat",
            "tool_allow": spec.tool_allow,
            "hitl_policy": "auto_reject",
            "max_steps": spec.max_steps,
        });

        // Inherit the parent's filesystem grants.
        //
        // Without this a delegate is unusable: its session is brand new, so
        // `allowed_dirs_for_session` sees no `chat_dir`, no `cwd`, no
        // `working_dirs` and no `attached_paths`, and the allowed set collapses
        // to the daemon's own cache directories. Every specialist whose job is
        // to read the user's files — summarizer, locator, extractor, analyst —
        // would then be denied on its first read, under a `hitl_policy` that
        // turns that denial into a silent refusal rather than a prompt.
        //
        // Inherited rather than widened: a delegate gets exactly what the
        // session that asked for it already had, and nothing more. It cannot
        // reach anything its parent could not, and because these are copied at
        // spawn rather than shared, a grant revoked on the parent mid-run does
        // not retroactively change what an in-flight delegate may touch.
        for key in ["chat_dir", "cwd", "working_dirs", "attached_paths"] {
            if let Some(v) = parent_meta.get(key) {
                meta[key] = v.clone();
            }
        }
        // The chosen host, not the raw pin. `choose_host` drops a model pin
        // together with its provider when the provider is ineligible, because a
        // model chosen *for* one provider is not meaningful at another.
        if let Some(p) = host_provider.as_deref().filter(|p| !p.is_empty()) {
            meta["provider"] = json!(p);
        }
        if let Some(m) = host_model.as_deref().filter(|m| !m.is_empty()) {
            meta["model"] = json!(m);
        }
        if let Some(sp) = spec.system_prompt.as_deref().filter(|s| !s.is_empty()) {
            meta["persona_override"] = json!(sp);
        }
        if let Some(schema) = spec.response_schema.as_ref() {
            meta["response_schema"] = schema.clone();
        }
        // Always written, never left absent: an uncapped delegate is one nobody
        // is watching that can think for as long as it likes while blocking the
        // turn that called it and holding a concurrency permit. The definition's
        // own cap wins; otherwise the daemon default applies.
        let cap = spec.reasoning_cap.unwrap_or(
            crate::agent::tokens::ReasoningCap::ContextFraction(self.default_reasoning_fraction),
        );
        meta["reasoning_cap"] = serde_json::to_value(cap).unwrap_or(Value::Null);
        // Carried into the child's own metadata, because `choose_host` is not
        // the last word on which model this run uses. The turn loop re-resolves
        // the provider when the chosen one errors or is unhealthy at step 0, and
        // that re-resolution knows nothing about specialists — so a delegate
        // picked onto a permitted host could fail over onto a denied one and
        // spend exactly what the denylist exists to prevent. The loop consults
        // this at both resolution sites.
        if !self.model_deny.is_empty() {
            meta["subagent_model_deny"] = json!(self.model_deny);
        }
        // Delegates read and write the response cache; interactive chat does
        // not. A pipeline re-running a specialist over the same corpus pays for
        // identical calls otherwise, and only the delegate's *final* request is
        // cached — the one that carries no tools, so replaying it cannot claim
        // work that never happened.
        meta["response_cache"] = json!({"read": true, "write": true});
        if let Err(e) =
            sessions::update_session_config(&self.db, &child_id, &meta.to_string()).await
        {
            return Err(SpawnRefusal::Storage(e.to_string()));
        }

        // A tag, not a foreign key: deleting the parent must not delete the
        // transcript that is often the actual output.
        if let Err(e) =
            sessions::set_parent(&self.db, &child_id, &spec.parent_session_id, &app_id).await
        {
            tracing::warn!("failed to tag delegate {child_id}'s parent: {e}");
        }

        // `subagent` has been a permitted `trigger_type` since the table was
        // written and has never had a producer. This is it.
        let exec_id = Uuid::new_v4().to_string();
        if let Err(e) =
            execution::insert_execution(&self.db, &exec_id, &child_id, "subagent", Some(&spec.name))
                .await
        {
            tracing::warn!("failed to record delegate execution {exec_id}: {e}");
        }

        self.notify(&agent, &spec, &child_id, "started", None);

        // Background, so a delegate never puts the user's own next message
        // behind it. The queue is work-conserving, so a lone parent and its
        // children still use every slot the endpoint has.
        //
        // Bounded in wall-clock time, which nothing else does: the turn
        // machinery has no timeout, and `call_specialist` blocks the parent's
        // tool call — so without this a slow delegate hangs the user's turn with
        // no recourse, while holding one of a small number of permits. On expiry
        // the child is cancelled rather than merely abandoned, or it would keep
        // spending on an answer nobody is waiting for any more.
        let outcome = match tokio::time::timeout(
            self.timeout.min(deadline.saturating_duration_since(tokio::time::Instant::now())),
            agent.run_turn_and_wait(&child_id, &spec.prompt, Priority::Background),
        )
        .await
        {
            Ok(res) => res,
            Err(_) => {
                agent.cancel(&child_id).await;
                Err(format!(
                    "the specialist ran longer than {}s and was stopped",
                    self.timeout.as_secs()
                ))
            }
        };

        // Mid-run notices the watcher collected — a failover, a classified
        // provider error. Previously discarded with the rest of the child's
        // event stream, which is how a delegate could run somewhere other than
        // where it reported.
        if let Ok(collected) = &outcome {
            for n in collected {
                if !notes.contains(n) {
                    notes.push(n.clone());
                }
            }
        }

        let result = match outcome {
            Ok(_) => match sessions::last_assistant_text(&self.db, &child_id).await {
                Ok(Some(text)) => Ok(text),
                Ok(None) => Err("the specialist produced no answer".to_string()),
                Err(e) => Err(format!("could not read the specialist's answer: {e}")),
            },
            Err(msg) => Err(msg),
        };

        let (status, summary, error) = match &result {
            Ok(text) => ("completed", Some(short(text)), None),
            Err(msg) => ("failed", None, Some(msg.as_str())),
        };
        if let Err(e) =
            execution::update_execution_status(&self.db, &exec_id, status, summary.as_deref(), error)
                .await
        {
            tracing::warn!("failed to close delegate execution {exec_id}: {e}");
        }

        self.notify(
            &agent,
            &spec,
            &child_id,
            status,
            result.as_ref().err().map(String::as_str),
        );

        // Read back what the turn actually ran on rather than trusting the
        // pick: a mid-turn failover can move it, and the point of reporting the
        // host is that it is true.
        let host = self.host_actually_used(&child_id, host_provider, host_model).await;

        Ok(result.map(|answer| DelegateOutcome {
            answer,
            host,
            notes,
        }))
    }

    /// Run several delegates concurrently, one per spec.
    ///
    /// Each takes its own permit, so `max_concurrent_specialists` bounds the
    /// fan-out with no second limiter to keep in agreement — a fan-out of ten
    /// against a cap of three simply runs three at a time.
    ///
    /// **A failure is per element, never for the batch.** One document that
    /// could not be parsed should cost the caller that row, not the other nine:
    /// a caller handed an error learns nothing about the nine that worked, and
    /// re-running to find out is the expense the fan-out existed to avoid.
    pub async fn run_many(
        &self,
        specs: Vec<DelegateRun>,
    ) -> Vec<Result<Result<DelegateOutcome, String>, SpawnRefusal>> {
        let deadline = tokio::time::Instant::now() + self.batch_timeout();
        futures::future::join_all(specs.into_iter().map(|s| self.run_by(s, deadline))).await
    }

    /// How long a whole fan-out may take.
    ///
    /// A multiple of the per-run ceiling rather than equal to it: every child
    /// deserves a real chance to finish, and with a small permit count they
    /// cannot all run at once. Three waves is the compromise — long enough that
    /// a normal fan-out completes, short enough that the parent's turn is bounded
    /// in minutes rather than in however many sources a model chose to pass.
    fn batch_timeout(&self) -> std::time::Duration {
        self.timeout.saturating_mul(FAN_OUT_DEADLINE_WAVES)
    }

    /// What the child's session says it ran on, once it has finished.
    async fn host_actually_used(
        &self,
        child_id: &str,
        picked_provider: Option<String>,
        picked_model: Option<String>,
    ) -> HostChoice {
        let meta: Value = sessions::get_session(&self.db, child_id)
            .await
            .ok()
            .flatten()
            .and_then(|r| r.metadata)
            .and_then(|m| serde_json::from_str(&m).ok())
            .unwrap_or_else(|| json!({}));

        HostChoice {
            provider: meta
                .get("provider")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or(picked_provider)
                .unwrap_or_else(|| "default".to_string()),
            model: meta
                .get("model")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or(picked_model)
                .unwrap_or_else(|| "default".to_string()),
            instances: 1,
            tokens: sessions::token_total(&self.db, child_id).await.unwrap_or(0),
        }
    }

    /// Tell whoever is watching the parent that a delegate changed state.
    ///
    /// Best-effort by design: a detached parent (a job, a scheduled run) has no
    /// stream, and a delegate must not be held up by the absence of an
    /// audience.
    fn notify(
        &self,
        agent: &Arc<Agent>,
        spec: &DelegateRun,
        child_id: &str,
        status: &str,
        error: Option<&str>,
    ) {
        agent.emit_to(
            &spec.parent_session_id,
            SSEEvent {
                event_type: SSEEventType::SubagentStatus,
                tool_name: Some(spec.name.clone()),
                content: Some(status.to_string()),
                // The *child's* id, deliberately: this event is how a client
                // offers a click-through into the delegate's own transcript.
                session_id: Some(child_id.to_string()),
                error_message: error.map(str::to_string),
                ..Default::default()
            },
        );
    }
}

/// A one-line form of a prompt or answer, for session names and audit
/// summaries.
fn short(text: &str) -> String {
    const MAX: usize = 80;
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}...", &flat[..cut]),
        None => flat,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> SqlitePool {
        SqlitePool::connect("sqlite::memory:").await.unwrap()
    }

    /// `MAX_FAN_OUT` is a spend ceiling; this is the one that reflects what a
    /// batch can actually finish, and `specialists::server` refuses above it
    /// rather than letting the shared deadline time out the overflow one
    /// element at a time.
    #[tokio::test]
    async fn completable_fan_out_follows_the_concurrency_limit() {
        let pool = test_pool().await;

        // Default config: 3 concurrent x 3 deadline waves.
        let o = Orchestrator::new(pool.clone(), 3, 0.25, Vec::new(), 300);
        assert_eq!(o.limit(), 3);
        assert_eq!(o.completable_fan_out(), 9);

        // Raising the concurrency limit raises it, which is what the refusal
        // message tells the caller to do.
        let o = Orchestrator::new(pool.clone(), 8, 0.25, Vec::new(), 300);
        assert_eq!(o.completable_fan_out(), 24);

        // `new` floors the limit at 1, so this can never be zero — a zero cap
        // would refuse every fan-out, including a two-source one.
        let o = Orchestrator::new(pool, 0, 0.25, Vec::new(), 300);
        assert_eq!(o.limit(), 1);
        assert_eq!(o.completable_fan_out(), FAN_OUT_DEADLINE_WAVES as usize);
    }

    #[test]
    fn short_collapses_whitespace_and_bounds_length() {
        assert_eq!(short("a\n\n  b\tc"), "a b c");
        let long = "word ".repeat(60);
        let s = short(&long);
        assert!(s.chars().count() <= 83, "got {} chars", s.chars().count());
        assert!(s.ends_with("..."));
    }

    /// A multi-byte prompt must not be cut mid-character. `char_indices` is
    /// what makes this safe; slicing by byte count would panic.
    #[test]
    fn short_never_splits_a_multibyte_character() {
        let text = "日本語テキスト".repeat(40);
        let s = short(&text);
        assert!(s.ends_with("..."));
    }
}
