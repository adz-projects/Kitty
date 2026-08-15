pub mod compaction;
pub mod context;
pub(crate) mod json_extract;
pub mod loop_;
pub mod memory;
pub mod reasoning_models;
pub mod sandbox;
pub mod summarizer_chain;
pub mod tokens;
pub mod types;

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::sync::{mpsc, Mutex, Notify};
use uuid::Uuid;

use crate::config::BigTinyConfig;
use crate::hitl::manager::HITLManager;
use crate::mcp::MCPManager;
use crate::provider::router::ProviderRouter;
use crate::server::events::{SSEEvent, SSEEventType};

type PathwayEngine = adaptive_pathway::engine::PathwayEngine;

use self::context::builder::ContextBuilder;
use self::context::stats::SessionStats;
use self::loop_::AgentLoop;
use self::memory::PreflightCounters;
use self::summarizer_chain::SummarizerChain;

/// Cross-session state the HTTP routes need: in-flight turn handles (for
/// `/cancel`), and the HITL pause/resume map (for `/approve`) — the pieces
/// `AgentLoop` alone doesn't own since it's constructed fresh per turn (see
/// `run_turn`). Ports the shape of `plugins/bigtiny/bigtiny/agent/loop.py`'s
/// `Agent` class.
pub struct Agent {
    db: SqlitePool,
    router: Arc<ProviderRouter>,
    mcp: Arc<MCPManager>,
    hitl: Arc<Mutex<HITLManager>>,
    hitl_notifies: Arc<DashMap<String, Arc<Notify>>>,
    /// Keyed by session id. The `Uuid` identifies *this specific turn* so the
    /// disconnect watcher spawned in `run_turn` can never abort a later,
    /// unrelated turn for the same session (see that method's doc comment).
    /// The sender clone lets `cancel()` emit the terminal `Cancelled`
    /// session-status frame before aborting the turn; it is dropped with the
    /// entry, so the stream still closes exactly when the turn ends.
    tasks: DashMap<String, (Uuid, tokio::task::JoinHandle<()>, mpsc::UnboundedSender<SSEEvent>)>,
    summarizer: Arc<SummarizerChain>,
    config: BigTinyConfig,
    /// Daemon-wide pre-flight recall counters, shared across every per-turn
    /// `AgentLoop` (each is built fresh). Read by `GET /api/memory/stats`.
    preflight: Arc<PreflightCounters>,
    /// BigTiny's app-data directory (`RunOptions::data_dir` — respects
    /// `BIGTINY_DATA_DIR`), threaded into every `AgentLoop` as the sandbox's
    /// always-allowed cache dir. See `sandbox::CACHE_DIR`'s doc comment.
    cache_dir: String,
    /// Behavioral-memory engine. `None` when disabled.
    pathway: Option<Arc<PathwayEngine>>,
    /// Abort channel for the pathway background task. Set by `lib.rs::run()`
    /// after spawning `adaptive_pathway::background::run()`; signalled during
    /// `Agent::shutdown()` before cancelling in-flight turns.
    pathway_shutdown: Option<tokio::sync::watch::Sender<bool>>,
    /// Sessions already warned about a pinned-provider mismatch (see
    /// `AgentLoop::provider_mismatch_warned`) — daemon-lifetime, since each
    /// `AgentLoop` is rebuilt per turn.
    provider_mismatch_warned: Arc<DashMap<String, ()>>,
}

/// Removes the turn's `tasks` entry when the turn task ends — normally *or*
/// by panic. Lives inside the spawned task; tokio catches a panic at the
/// task boundary, so unwinding drops this guard and the cleanup still runs.
struct TurnCleanup {
    agent: Arc<Agent>,
    session_id: String,
    turn_token: Uuid,
}

impl Drop for TurnCleanup {
    fn drop(&mut self) {
        self.agent
            .tasks
            .remove_if(&self.session_id, |_, (tok, _, _)| *tok == self.turn_token);
    }
}

impl Agent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: SqlitePool,
        router: Arc<ProviderRouter>,
        mcp: Arc<MCPManager>,
        hitl: Arc<Mutex<HITLManager>>,
        summarizer: Arc<SummarizerChain>,
        config: BigTinyConfig,
        cache_dir: String,
        pathway: Option<Arc<PathwayEngine>>,
        pathway_shutdown: Option<tokio::sync::watch::Sender<bool>>,
    ) -> Self {
        Self {
            db,
            router,
            mcp,
            hitl,
            hitl_notifies: Arc::new(DashMap::new()),
            tasks: DashMap::new(),
            summarizer,
            preflight: Arc::new(PreflightCounters::new()),
            config,
            cache_dir,
            pathway,
            pathway_shutdown,
            provider_mismatch_warned: Arc::new(DashMap::new()),
        }
    }

    pub fn hitl(&self) -> &Arc<Mutex<HITLManager>> {
        &self.hitl
    }

    pub fn mcp(&self) -> &Arc<MCPManager> {
        &self.mcp
    }

    pub fn router(&self) -> &Arc<ProviderRouter> {
        &self.router
    }

    pub fn summarizer(&self) -> &Arc<SummarizerChain> {
        &self.summarizer
    }

    pub fn config(&self) -> &BigTinyConfig {
        &self.config
    }

    /// (total, injected) pre-flight recall counters — backs `GET
    /// /api/memory/stats`.
    pub fn preflight_snapshot(&self) -> (u64, u64) {
        self.preflight.snapshot()
    }

    pub fn preflight(&self) -> &Arc<PreflightCounters> {
        &self.preflight
    }

    /// Construct a fresh per-turn `AgentLoop`. `AgentLoop` holds no
    /// meaningful session-scoped state of its own (session id is passed to
    /// its methods, not baked into construction) so building one per turn is
    /// cheap and avoids needing to share a single `AgentLoop` — and its
    /// `&mut self` — across concurrent turns.
    fn build_loop(&self) -> AgentLoop {
        let context = ContextBuilder::new(
            self.db.clone(),
            self.config.token_management.clone(),
            self.config.summarizer.reserve_exchanges,
        );
        let stats = SessionStats::new(self.db.clone());
        AgentLoop::new(
            self.router.clone(),
            self.hitl.clone(),
            self.mcp.clone(),
            self.hitl_notifies.clone(),
            context,
            stats,
            self.summarizer.clone(),
            self.config.summarizer.clone(),
            self.config.memory.clone(),
            self.preflight.clone(),
            self.config.agent.max_concurrent_tool_calls.max(1) as usize,
            self.cache_dir.clone(),
            self.config.fallback.clone(),
            self.config.agent.sandbox_strict,
            self.pathway.clone(),
            self.config.pathway.clone(),
            self.provider_mismatch_warned.clone(),
        )
    }

    /// Run one turn for `session_id` in the background, streaming events over
    /// `tx`. Tracks the spawned task so `cancel()` can abort it (matching
    /// Python's `asyncio.Task.cancel()` semantics — BigTiny's turn loop has
    /// no cooperative cancellation checkpoints of its own beyond what
    /// dropping the task achieves).
    ///
    /// Returns `Err` without starting anything if a turn for this session is
    /// already in flight. This isn't just a nicety: `tasks` is keyed by
    /// `session_id`, and two concurrent turns for the same session (two
    /// racing `/send` calls) would let whichever one finishes *first*
    /// overwrite-then-remove the *other* still-running turn's entry —
    /// `cancel()` would then find nothing to abort for the turn that's
    /// actually still running. Reserving the slot via `DashMap::entry`
    /// (atomic check-and-insert under that shard's lock) closes both the
    /// double-spawn race and that stale-removal race at once.
    pub fn run_turn(
        self: &Arc<Self>,
        session_id: String,
        user_message: String,
        images: Option<Vec<Value>>,
        provider_override: Option<String>,
        tx: mpsc::UnboundedSender<SSEEvent>,
    ) -> Result<(), String> {
        let this = self.clone();
        let turn_token = Uuid::new_v4();

        // A finished-but-never-removed entry (only possible if something
        // outside the turn task's drop guard went wrong) must not wedge the
        // session into a permanent "turn in progress" — treat it as
        // replaceable. Atomic under the shard lock via `remove_if`.
        self.tasks
            .remove_if(&session_id, |_, (_, handle, _)| handle.is_finished());

        match self.tasks.entry(session_id.clone()) {
            dashmap::mapref::entry::Entry::Occupied(_) => Err(format!(
                "Session {session_id} already has a turn in progress"
            )),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                // Disconnect watcher: `tx.closed()` resolves once the SSE
                // receiver (owned by the axum response body) is dropped —
                // i.e. when the client hangs up. We don't cancel immediately:
                // we wait out the grace window and then only cancel if *this
                // exact turn* (matched by `turn_token`) is still the one
                // registered for the session. This is what makes a dropped
                // mobile connection recoverable rather than an instant kill:
                // see `AgentConfig::disconnect_grace_secs`.
                //
                // The watcher must also stop when the turn simply *finishes*,
                // hence `turn_done`. Without it this deadlocked every SSE
                // response: the watcher held a live `tx` clone, so the channel
                // never closed, so the response body never ended, so the
                // receiver was never dropped, so `closed()` never resolved.
                // A client reading to EOF (rather than stopping at `llm_stop`)
                // would hang forever, and every turn leaked a task.
                let watch_tx = tx.clone();
                let watcher_agent = this.clone();
                let watcher_session_id = session_id.clone();
                let grace = Duration::from_secs(this.config.agent.disconnect_grace_secs.max(1));
                let (turn_done, turn_done_rx) = tokio::sync::oneshot::channel::<()>();
                tokio::spawn(async move {
                    tokio::select! {
                        _ = watch_tx.closed() => {}
                        // Resolves (as `Err`) when the turn task drops its end,
                        // whether it completed or panicked. Either way there is
                        // nothing left to cancel.
                        _ = turn_done_rx => return,
                    }
                    // Release the last non-loop sender before sleeping, so a
                    // turn that ends during the grace window can still close
                    // its stream.
                    drop(watch_tx);
                    tokio::time::sleep(grace).await;
                    watcher_agent
                        .cancel_if_current(&watcher_session_id, turn_token)
                        .await;
                });

                let cleanup_session_id = session_id.clone();
                let cancel_tx = tx.clone();
                let handle = tokio::spawn(async move {
                    // Drop-guard cleanup: if `run()` panics, everything after
                    // it in this task used to be skipped — leaking the
                    // `tasks` entry and wedging the session into a permanent
                    // "turn in progress". The guard removes the entry on
                    // unwind too (tokio catches the panic at the task
                    // boundary and reports it via the JoinHandle).
                    let _cleanup = TurnCleanup {
                        agent: this.clone(),
                        session_id: cleanup_session_id,
                        turn_token,
                    };
                    let mut agent_loop = this.build_loop();
                    agent_loop
                        .run(
                            &session_id,
                            &user_message,
                            tx,
                            provider_override.as_deref(),
                            images,
                        )
                        .await;
                    drop(turn_done);
                });
                entry.insert((turn_token, handle, cancel_tx));
                Ok(())
            }
        }
    }

    /// Run one turn to completion in the *caller's* task rather than
    /// spawning — used by `RecipeEngine::execute`/the scheduler, which
    /// (like Python's `await self.agent.run(...)`) need the session fully
    /// populated before they return, not a fire-and-forget stream. SSE
    /// events are drained by a watcher (no other receiver reads them),
    /// matching Python's `_noop_callback` default.
    ///
    /// Returns the turn's outcome, derived from the terminal frame: a
    /// terminal `Error` event is `Err(message)`, anything else (the
    /// `SessionStatus "idle"` success frame, or the channel closing without
    /// a terminal frame) is `Ok(())`. Without this the scheduler recorded
    /// provider-failed runs as `'completed'` in `execution_history`.
    pub async fn run_turn_and_wait(
        self: &Arc<Self>,
        session_id: &str,
        user_message: &str,
    ) -> Result<(), String> {
        let (tx, mut rx) = mpsc::unbounded_channel::<SSEEvent>();
        // `is_last` is only set on terminal frames (Error / SessionStatus) —
        // `LlmStop` mid-turn carries the default `false`.
        let watcher = tokio::spawn(async move {
            let mut failure: Option<String> = None;
            while let Some(ev) = rx.recv().await {
                if ev.is_last {
                    if ev.event_type == SSEEventType::Error {
                        failure = Some(
                            ev.error_message
                                .clone()
                                .or(ev.content.clone())
                                .unwrap_or_else(|| "turn failed".to_string()),
                        );
                    }
                    break;
                }
            }
            failure
        });
        let mut agent_loop = self.build_loop();
        agent_loop
            .run(session_id, user_message, tx, None, None)
            .await;
        match watcher.await {
            Ok(Some(msg)) => Err(msg),
            _ => Ok(()),
        }
    }

    /// Abort the in-flight turn for `session_id`, if any. Emits the terminal
    /// `Cancelled` session-status frame *before* aborting: Kitty's stream
    /// layer (`src-tauri/src/bigtiny/stream.rs`) matches
    /// `session_status == "Cancelled"` to report `stopReason: "cancelled"` —
    /// without this frame every user cancel was indistinguishable from a
    /// normal `end_turn`. After the frame, the aborted task's sender (and
    /// this clone) drop, so the route's stream ends right behind it.
    pub async fn cancel(&self, session_id: &str) {
        if let Some((_, (_, handle, tx))) = self.tasks.remove(session_id) {
            Self::emit_cancelled(&tx, session_id);
            handle.abort();
        }
    }

    /// The terminal frame for a cancelled turn. A send failure is expected
    /// and harmless — the disconnect watcher (`cancel_if_current`) fires
    /// precisely when the receiver is already gone.
    fn emit_cancelled(tx: &mpsc::UnboundedSender<SSEEvent>, session_id: &str) {
        let _ = tx.send(SSEEvent {
            event_type: SSEEventType::SessionStatus,
            session_id: Some(session_id.to_string()),
            content: Some("Cancelled".into()),
            is_last: true,
            ..Default::default()
        });
    }

    /// Cancel the in-flight turn for `session_id` only if it's still the
    /// specific turn identified by `turn_token` — used by the disconnect
    /// watcher in `run_turn` so a stale watcher for an already-finished (or
    /// already-replaced) turn can never abort a different, later turn. See
    /// that method's doc comment for why this distinction matters.
    async fn cancel_if_current(&self, session_id: &str, turn_token: Uuid) {
        if let Some((_, (_, handle, tx))) = self
            .tasks
            .remove_if(session_id, |_, (tok, _, _)| *tok == turn_token)
        {
            Self::emit_cancelled(&tx, session_id);
            handle.abort();
        }
    }

    /// Wake a tool call paused on `needs_approval` for `action_id`, after the
    /// caller has already recorded the decision via `hitl.record_decision`.
    pub fn resolve_approval(&self, action_id: &str) {
        if let Some((_, notify)) = self.hitl_notifies.remove(action_id) {
            notify.notify_one();
        }
    }

    /// Cancel every in-flight turn — called during daemon shutdown.
    pub async fn shutdown(&self) {
        // Abort the pathway background task first, so it stops touching the DB
        // before we cancel turns (which may still read pathway beliefs).
        if let Some(tx) = &self.pathway_shutdown {
            let _ = tx.send(true);
        }
        let ids: Vec<String> = self.tasks.iter().map(|e| e.key().clone()).collect();
        for id in ids {
            self.cancel(&id).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a real `Agent` against an in-memory, migrated DB — cheap
    /// enough to construct per test, and lets these tests exercise
    /// `cancel_if_current` (a private method) directly rather than through
    /// the full `run_turn`/HTTP/provider machinery, since the thing under
    /// test is purely the turn-token bookkeeping in `self.tasks`, not the
    /// agent loop itself.
    async fn test_agent() -> Arc<Agent> {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();

        let config = BigTinyConfig::default();
        let router = Arc::new(ProviderRouter::new(config.cache.clone()));
        let mcp = Arc::new(MCPManager::new(pool.clone(), None));
        let hitl = Arc::new(Mutex::new(HITLManager::new(pool.clone(), config.hitl.clone())));
        #[cfg(feature = "local-engine")]
        let summarizer = Arc::new(SummarizerChain::new(None, router.clone(), config.summarizer.clone()));
        #[cfg(not(feature = "local-engine"))]
        let summarizer = Arc::new(SummarizerChain::new(router.clone(), config.summarizer.clone()));
        Arc::new(Agent::new(
            pool,
            router,
            mcp,
            hitl,
            summarizer,
            config,
            std::env::temp_dir().to_string_lossy().into_owned(),
            None,
            None,
        ))
    }

    /// Inserts a task that never finishes on its own, standing in for an
    /// in-flight turn, so tests can assert on whether `cancel_if_current`
    /// actually aborted it. Returns the turn token and the channel receiver
    /// (kept alive by the caller so sends don't error for the wrong reason).
    fn insert_fake_turn(
        agent: &Agent,
        session_id: &str,
    ) -> (Uuid, mpsc::UnboundedReceiver<SSEEvent>) {
        let token = Uuid::new_v4();
        let handle = tokio::spawn(std::future::pending::<()>());
        let (tx, rx) = mpsc::unbounded_channel::<SSEEvent>();
        agent.tasks.insert(session_id.to_string(), (token, handle, tx));
        (token, rx)
    }

    #[tokio::test]
    async fn cancel_if_current_is_a_noop_for_a_stale_token() {
        let agent = test_agent().await;
        let (real_token, _rx) = insert_fake_turn(&agent, "sess-1");
        let stale_token = Uuid::new_v4();

        // Simulates the disconnect watcher for a turn that already finished
        // (or was replaced by a later turn) firing after the fact: it must
        // not touch whatever is currently registered for this session.
        agent.cancel_if_current("sess-1", stale_token).await;

        let entry = agent.tasks.get("sess-1").expect("entry must survive");
        assert_eq!(entry.0, real_token);
        assert!(!entry.1.is_finished());
    }

    #[tokio::test]
    async fn cancel_if_current_aborts_the_matching_turn() {
        let agent = test_agent().await;
        let (token, _rx) = insert_fake_turn(&agent, "sess-1");

        agent.cancel_if_current("sess-1", token).await;

        assert!(agent.tasks.get("sess-1").is_none());
    }

    #[tokio::test]
    async fn cancel_if_current_does_not_touch_other_sessions() {
        let agent = test_agent().await;
        let (token_a, _rx_a) = insert_fake_turn(&agent, "sess-a");
        let (_token_b, _rx_b) = insert_fake_turn(&agent, "sess-b");

        agent.cancel_if_current("sess-a", token_a).await;

        assert!(agent.tasks.get("sess-a").is_none());
        assert!(agent.tasks.get("sess-b").is_some());
    }

    /// Regression: the `/cancel` path used to abort the turn task and drop
    /// the SSE sender without ever emitting a cancelled status, so Kitty's
    /// stream layer (which matches `session_status == "Cancelled"`) reported
    /// every user cancel as `stopReason: "end_turn"`. The terminal
    /// `Cancelled` frame must arrive before the stream closes.
    #[tokio::test]
    async fn cancel_emits_a_terminal_cancelled_frame_before_closing() {
        let agent = test_agent().await;
        let (_token, mut rx) = insert_fake_turn(&agent, "sess-1");

        agent.cancel("sess-1").await;

        let frame = rx.recv().await.expect("a Cancelled frame must be sent");
        assert_eq!(frame.event_type, SSEEventType::SessionStatus);
        assert_eq!(frame.content.as_deref(), Some("Cancelled"));
        assert!(frame.is_last, "the Cancelled frame is the terminal one");
        assert_eq!(frame.session_id.as_deref(), Some("sess-1"));
        // ...and the stream closes right behind it (every sender dropped).
        assert!(rx.recv().await.is_none());
    }

    /// Regression: a panic in `agent_loop.run()` used to skip the
    /// `tasks.remove_if` cleanup, permanently wedging the session as "turn
    /// in progress". The drop guard must remove the entry even when the turn
    /// task ends abnormally (verified here by dropping the guard directly —
    /// the same thing unwinding does).
    #[tokio::test]
    async fn turn_cleanup_guard_removes_the_entry_on_drop() {
        let agent = test_agent().await;
        let (token, _rx) = insert_fake_turn(&agent, "sess-1");

        {
            let _guard = TurnCleanup {
                agent: agent.clone(),
                session_id: "sess-1".to_string(),
                turn_token: token,
            };
            // guard dropped here — the panic path in the turn task
        }

        assert!(agent.tasks.get("sess-1").is_none());
    }

    /// A guard for a *stale* token must not remove a newer turn's entry.
    #[tokio::test]
    async fn turn_cleanup_guard_ignores_a_replaced_entry() {
        let agent = test_agent().await;
        let (new_token, _rx) = insert_fake_turn(&agent, "sess-1");

        drop(TurnCleanup {
            agent: agent.clone(),
            session_id: "sess-1".to_string(),
            turn_token: Uuid::new_v4(), // stale token, not the registered one
        });

        let entry = agent.tasks.get("sess-1").expect("entry must survive");
        assert_eq!(entry.0, new_token);
    }

    /// A finished-but-unremoved entry must not block the next turn forever —
    /// `run_turn` treats it as replaceable.
    #[tokio::test]
    async fn run_turn_replaces_a_finished_entry() {
        let agent = test_agent().await;
        let token = Uuid::new_v4();
        let (tx, _rx) = mpsc::unbounded_channel::<SSEEvent>();
        agent.tasks.insert(
            "no-such-session".to_string(),
            (token, tokio::spawn(async {}), tx),
        );
        // Let the runtime run the empty task to completion.
        for _ in 0..100 {
            if agent.tasks.get("no-such-session").unwrap().1.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(agent.tasks.get("no-such-session").unwrap().1.is_finished());

        let (tx2, _rx2) = mpsc::unbounded_channel::<SSEEvent>();
        let result = agent.run_turn("no-such-session".into(), "hi".into(), None, None, tx2);
        assert!(result.is_ok(), "a finished entry must be replaceable: {result:?}");
    }

    /// The SSE channel must close once the turn is over.
    ///
    /// It used to deadlock: the disconnect watcher held a `tx` clone until the
    /// *receiver* was dropped, but axum only drops the receiver when the
    /// response body ends, which only happens when every sender is gone. Any
    /// client reading to end-of-stream — rather than stopping the moment it
    /// sees `llm_stop` — hung forever, and each turn leaked a watcher task.
    ///
    /// The turn itself fails immediately here (no such session, no providers
    /// registered); that's fine, since what's under test is channel teardown,
    /// which must hold regardless of how the turn ended.
    #[tokio::test]
    async fn the_event_channel_closes_when_the_turn_ends() {
        let agent = test_agent().await;
        let (tx, mut rx) = mpsc::unbounded_channel::<SSEEvent>();
        agent
            .run_turn("no-such-session".into(), "hi".into(), None, None, tx)
            .expect("first turn for a session is always accepted");

        // Drain to the end. `recv()` returning `None` *is* the assertion:
        // before the fix this future never resolved.
        // Generous: the real figure is well under a millisecond, so 10s only
        // ever trips on the deadlock this guards against.
        let drained = tokio::time::timeout(Duration::from_secs(10), async {
            while rx.recv().await.is_some() {}
        })
        .await;
        assert!(
            drained.is_ok(),
            "the SSE channel never closed — the disconnect watcher is holding a sender"
        );
    }
}
