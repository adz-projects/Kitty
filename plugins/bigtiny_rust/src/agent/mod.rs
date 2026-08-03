pub mod compaction;
pub mod context;
pub mod loop_;
pub mod sandbox;
pub mod summarizer;
pub mod tokens;
pub mod types;

use std::sync::Arc;

use dashmap::DashMap;
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::sync::{mpsc, Mutex, Notify};

use crate::config::BigTinyConfig;
use crate::hitl::manager::HITLManager;
use crate::mcp::MCPManager;
use crate::provider::router::ProviderRouter;
use crate::server::events::SSEEvent;

use self::context::builder::ContextBuilder;
use self::context::stats::SessionStats;
use self::loop_::AgentLoop;
use self::summarizer::SummarizerClient;

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
    tasks: DashMap<String, tokio::task::JoinHandle<()>>,
    summarizer: Arc<SummarizerClient>,
    config: BigTinyConfig,
    /// BigTiny's app-data directory (`RunOptions::data_dir` — respects
    /// `BIGTINY_DATA_DIR`), threaded into every `AgentLoop` as the sandbox's
    /// always-allowed cache dir. See `sandbox::CACHE_DIR`'s doc comment.
    cache_dir: String,
}

impl Agent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: SqlitePool,
        router: Arc<ProviderRouter>,
        mcp: Arc<MCPManager>,
        hitl: Arc<Mutex<HITLManager>>,
        summarizer: Arc<SummarizerClient>,
        config: BigTinyConfig,
        cache_dir: String,
    ) -> Self {
        Self {
            db,
            router,
            mcp,
            hitl,
            hitl_notifies: Arc::new(DashMap::new()),
            tasks: DashMap::new(),
            summarizer,
            config,
            cache_dir,
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
            self.config.agent.max_concurrent_tool_calls.max(1) as usize,
            self.cache_dir.clone(),
            self.config.fallback.clone(),
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

        match self.tasks.entry(session_id.clone()) {
            dashmap::mapref::entry::Entry::Occupied(_) => Err(format!(
                "Session {session_id} already has a turn in progress"
            )),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let cleanup_session_id = session_id.clone();
                let handle = tokio::spawn(async move {
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
                    this.tasks.remove(&cleanup_session_id);
                });
                entry.insert(handle);
                Ok(())
            }
        }
    }

    /// Run one turn to completion in the *caller's* task rather than
    /// spawning — used by `RecipeEngine::execute`/the scheduler, which
    /// (like Python's `await self.agent.run(...)`) need the session fully
    /// populated before they return, not a fire-and-forget stream. SSE
    /// events are discarded (no receiver reads them), matching Python's
    /// `_noop_callback` default.
    pub async fn run_turn_and_wait(self: &Arc<Self>, session_id: &str, user_message: &str) {
        let (tx, _rx) = mpsc::unbounded_channel::<SSEEvent>();
        let mut agent_loop = self.build_loop();
        agent_loop
            .run(session_id, user_message, tx, None, None)
            .await;
    }

    /// Abort the in-flight turn for `session_id`, if any. The aborted task's
    /// SSE sender is simply dropped — the route holding the receiver end sees
    /// the stream end and closes the response; there is no explicit
    /// "cancelled" event on this path (unlike a cooperative
    /// `POST /cancel`-triggered stop inside the loop itself).
    pub async fn cancel(&self, session_id: &str) {
        if let Some((_, handle)) = self.tasks.remove(session_id) {
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
        let ids: Vec<String> = self.tasks.iter().map(|e| e.key().clone()).collect();
        for id in ids {
            self.cancel(&id).await;
        }
    }
}
