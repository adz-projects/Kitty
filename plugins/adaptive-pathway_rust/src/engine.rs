//! The `PathwayEngine`: the shared, in-process behavioral-memory engine.
//! Owns the `Pathway`-db pool, the embedding provider, and per-session pause
//! state. Reads are direct in-process calls; writes the model chooses to make
//! go through MCP tools.

use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::embed::provider::EmbeddingProvider;
use crate::error::Result;
use crate::store::Db;
use crate::vector::ops;

/// The three cadence-gated anti-sycophancy signals a turn may carry,
/// alongside the recalled beliefs themselves. Gathered once by
/// `PathwayEngine::turn_signals` and rendered by either
/// `antisycophancy::render_block` (system-block path) or
/// `recall::render_reflection_block` (thought-seed path) -- the two framings
/// differ, the underlying signals must not.
struct TurnSignals {
    worth_testing: Option<String>,
    unsure: Option<String>,
    check: Option<String>,
}

pub struct PathwayEngine {
    pub db: Db,
    pub cfg: Config,
    pub embed: EmbeddingProvider,
    /// Mirrors `conversation_state.paused` for the hot path. `None` (absent)
    /// means configured/available; a paused session maps to `Some(true)`.
    paused_override: DashMap<String, bool>,
    /// Per-session learn lock, held for the duration of one learn pass.
    learn_locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// Global 1-permit semaphore around every `structured_chat`.
    chat_slot: Arc<Semaphore>,
    /// Previous turn's raw query embedding per session, for
    /// `TrajectoryConfig`'s momentum extrapolation in `recall`. In-memory
    /// only -- see `config::TrajectoryConfig`'s doc comment for why losing
    /// this across a restart is fine.
    trajectory_embeddings: DashMap<String, Vec<f32>>,
}

impl PathwayEngine {
    pub async fn open(path: &str, cfg: Config) -> Result<Arc<Self>> {
        let db = Db::open(path).await?;
        if db.sync_embedding_model_fingerprint(&cfg.embedding.ollama_model).await.unwrap_or(false) {
            tracing::info!(
                model = %cfg.embedding.ollama_model,
                "embedding model changed (or first run) -- stale beliefs will be re-embedded in the background"
            );
        }
        let embed = EmbeddingProvider::new(cfg.clone());
        Ok(Arc::new(Self {
            db,
            cfg,
            embed,
            paused_override: DashMap::new(),
            learn_locks: DashMap::new(),
            chat_slot: Arc::new(Semaphore::new(1)),
            trajectory_embeddings: DashMap::new(),
        }))
    }

    /// A per-session learn lock, released when the returned guard drops.
    ///
    /// The `Arc<Mutex<()>>` clone must happen in its own scope, separate
    /// from the `.await` below: `DashMap::entry(..).or_insert_with(..)`
    /// returns a `RefMut` that holds a lock on the map's internal shard for
    /// as long as it's alive. Awaiting `lock_owned()` while that `RefMut`
    /// is still in scope (the previous shape bound it to `guard` for the
    /// rest of the function) holds the shard lock across an `.await` point
    /// -- any other task touching a *different* session whose id happens to
    /// hash to the same shard blocks until this lock acquisition resolves,
    /// not just callers of the same session.
    pub async fn learn_lock(&self, session_id: &str) -> Result<tokio::sync::OwnedMutexGuard<()>> {
        let lock = {
            let guard = self
                .learn_locks
                .entry(session_id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())));
            guard.clone()
        };
        Ok(lock.lock_owned().await)
    }

    /// Whether learning should proceed for this session. Delegates straight
    /// to `is_paused` (memory-first, DB-fallback) rather than reading only
    /// the in-memory override map: after a daemon restart `paused_override`
    /// starts empty regardless of what's persisted, so a map-only check
    /// silently treated every session as unpaused post-restart -- exactly
    /// the sessions an incognito user is most likely to still be trusting.
    /// The extra DB read only happens on a learn pass (every few exchanges,
    /// at compaction, or at idle-sweep), never per-turn, so this isn't
    /// actually the hot path the old comment assumed.
    pub async fn learn_paused(&self, session_id: &str) -> Result<bool> {
        self.is_paused(session_id).await
    }

    /// Acquire the global chat permit (one structured_chat at a time).
    /// Handle to the global semaphore, for callers that want the permit
    /// helper.
    pub fn chat_semaphore(&self) -> Arc<Semaphore> {
        self.chat_slot.clone()
    }

    /// Drop `learn_locks` entries for sessions with no in-flight learn pass
    /// (`Arc::strong_count == 1` means only this map holds a reference --
    /// nobody has a clone parked in `lock_owned().await`). One entry per
    /// distinct session accumulates here forever otherwise, for the
    /// lifetime of a long-running daemon; called from the background
    /// maintenance sweep, not the hot path. `paused_override` isn't pruned
    /// here: it only grows when a user explicitly pauses a session, an
    /// intrinsically much smaller and slower-growing set than "every
    /// session that ever had a learn pass".
    pub fn prune_idle_learn_locks(&self) {
        self.learn_locks.retain(|_, lock| Arc::strong_count(lock) > 1);
    }

    #[cfg(test)]
    pub fn learn_locks_len(&self) -> usize {
        self.learn_locks.len()
    }

    pub async fn open_in_memory(cfg: Config) -> Result<Arc<Self>> {
        let db = Db::open_in_memory().await?;
        let _ = db.sync_embedding_model_fingerprint(&cfg.embedding.ollama_model).await;
        let embed = EmbeddingProvider::new(cfg.clone());
        Ok(Arc::new(Self {
            db,
            cfg,
            embed,
            paused_override: DashMap::new(),
            learn_locks: DashMap::new(),
            chat_slot: Arc::new(Semaphore::new(1)),
            trajectory_embeddings: DashMap::new(),
        }))
    }

    /// Is recall paused for `session_id`? Mirrors the DB (`conversation_state`)
    /// plus any in-memory override.
    pub async fn is_paused(&self, session_id: &str) -> Result<bool> {
        if let Some(p) = self.paused_override.get(session_id) {
            return Ok(*p);
        }
        self.db.is_paused(session_id).await
    }

    /// Set the per-session pause flag (incognito). Persisted to
    /// `conversation_state.paused` and mirrored to the DashMap for the hot
    /// path.
    pub async fn set_paused(&self, session_id: &str, paused: bool) -> Result<()> {
        self.db.set_paused(session_id, paused).await?;
        self.paused_override.insert(session_id.to_string(), paused);
        Ok(())
    }

    /// Build the full recall block for a turn: `[Working assumptions about
    /// you]` every turn, `[Worth testing this turn]` every turn a live
    /// assumption has surfaced, `[Where I'm unsure]` cadence-gated every 12
    /// exchanges, `[Check yourself]` on a detected plateau. `None` when
    /// there's nothing to say (paused, disabled, no beliefs) -- zero prompt
    /// delta, cache-preserving; callers must not distinguish `None` from
    /// `Some("")`.
    pub async fn recall(&self, session_id: &str, user_message: &str) -> Option<String> {
        let mut selected = self.select_for_turn(session_id, user_message).await?;
        let signals = self.turn_signals(session_id, &selected).await;
        let knows = crate::recall::render_knows(&mut selected);

        let block = crate::antisycophancy::render_block(
            &knows,
            signals.worth_testing,
            signals.unsure,
            signals.check,
        );
        if block.is_empty() {
            return None;
        }
        Some(crate::recall::cap_to_token_budget(block))
    }

    /// Thought-seeded variant of `recall`: the same belief-selection pass
    /// (`select_for_turn`) and the same three anti-sycophancy signals
    /// (`turn_signals`), rendered in inner-monologue voice with no bracketed
    /// headers (`recall::render_reflection_block`) -- meant to prefill a
    /// trailing assistant `<think>` turn, not a system message.
    ///
    /// This used to render only the fact list, dropping `[Worth testing]`/
    /// `[Where I'm unsure]`/`[Check yourself]` entirely, which made the
    /// seeded path strictly *more* sycophantic than the system block it
    /// replaces -- it kept the profile and discarded the machinery that
    /// makes the profile something to question. Both paths now carry the
    /// same four signals; `turn_signals` is shared precisely so they can't
    /// drift apart again.
    ///
    /// Same `None` conditions as `recall` (paused, disabled, no beliefs, no
    /// match). Callers are responsible for only invoking this instead of
    /// (never in addition to) `recall` when the active provider/model
    /// actually supports a trailing assistant-role prefill -- see
    /// `Provider::supports_assistant_prefill` and
    /// `reasoning_models::supports_reasoning` in `bigtiny_rust`.
    pub async fn recall_thought_seed(&self, session_id: &str, user_message: &str) -> Option<String> {
        let mut selected = self.select_for_turn(session_id, user_message).await?;
        let signals = self.turn_signals(session_id, &selected).await;
        let reflection = crate::recall::render_reflection(&mut selected);

        let block = crate::recall::render_reflection_block(
            &reflection,
            signals.worth_testing,
            signals.unsure,
            signals.check,
        );
        if block.is_empty() {
            return None;
        }
        Some(crate::recall::cap_reflection_to_token_budget(block))
    }

    /// The three anti-sycophancy signals both render paths carry, gathered
    /// once so `recall` and `recall_thought_seed` cannot diverge on which
    /// ones they surface. Cadence/eligibility gating lives in the individual
    /// `*_line` helpers, not here.
    async fn turn_signals(
        &self,
        session_id: &str,
        selected: &[crate::belief::SelectedBelief],
    ) -> TurnSignals {
        let top_belief_id = selected
            .iter()
            .max_by(|a, b| {
                a.effective_weight
                    .partial_cmp(&b.effective_weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|s| s.belief.id.clone());
        TurnSignals {
            worth_testing: self.worth_testing_line().await,
            unsure: self.unsure_line(session_id, selected).await,
            check: match &top_belief_id {
                Some(id) => self.check_yourself_line(id).await,
                None => None,
            },
        }
    }

    /// Shared candidate-selection pass behind `recall`/`recall_thought_seed`:
    /// pause gate, embedding-model-scoped candidates, suppression filter,
    /// trajectory-predicted query, domain inference, diffusion+DPP
    /// selection. `None` on any of: paused, no candidates, everything
    /// suppressed, or DPP selecting nothing.
    async fn select_for_turn(
        &self,
        session_id: &str,
        user_message: &str,
    ) -> Option<Vec<crate::belief::SelectedBelief>> {
        if self.is_paused(session_id).await.unwrap_or(false) {
            return None;
        }
        let candidates = self
            .db
            .list_recall_candidates(session_id, &self.cfg.embedding.ollama_model)
            .await
            .ok()?;
        if candidates.is_empty() {
            return None;
        }
        let suppressed = self
            .db
            .active_suppressed_text_hashes(chrono::Utc::now())
            .await
            .unwrap_or_default();
        let candidates = crate::belief::filter_suppressed(candidates, &suppressed);
        if candidates.is_empty() {
            return None;
        }

        let query: Vec<f32> = if user_message.trim().is_empty() {
            Vec::new()
        } else {
            self.embed.embed(user_message).await
        };
        // Momentum-extrapolate toward where the conversation is heading
        // (`TrajectoryConfig`) before using the query for anything --
        // `predicted_query` is what actually drives domain inference and
        // belief ranking, not the raw current-turn embedding.
        let predicted_query = self.trajectory_predict(session_id, &query);
        let query_domain = crate::domains::infer_query_domain(&candidates, &predicted_query);
        let cooccurrence = self.cooccurrence_adjacency(&candidates).await;

        let selected = crate::recall::select_beliefs_relevant(
            &candidates,
            &predicted_query,
            query_domain.as_deref(),
            &cooccurrence,
            &self.cfg,
        );
        if selected.is_empty() {
            None
        } else {
            Some(selected)
        }
    }

    /// Adjacency list over `candidates` (by index) of beliefs observed
    /// together in at least one `extract_and_record` batch — the
    /// co-occurrence edge set `recall::select_beliefs_relevant` diffuses
    /// alongside the cosine graph.
    ///
    /// Best-effort: a DB error yields an empty adjacency, degrading to pure
    /// cosine diffusion rather than failing the turn's recall, consistent
    /// with how every other optional signal in this path behaves. The read
    /// is a single indexed self-join bounded by the candidate count, on the
    /// same connection the candidate list just came from.
    async fn cooccurrence_adjacency(
        &self,
        candidates: &[crate::store::beliefs::Belief],
    ) -> Vec<Vec<usize>> {
        if self.cfg.diffusion.cooccurrence_weight <= 0.0 || candidates.len() < 2 {
            return Vec::new();
        }
        let ids: Vec<String> = candidates.iter().map(|b| b.id.clone()).collect();
        let pairs = match self.db.cooccurring_belief_pairs(&ids).await {
            Ok(p) if !p.is_empty() => p,
            _ => return Vec::new(),
        };
        let index_of: std::collections::HashMap<&str, usize> =
            ids.iter().enumerate().map(|(i, id)| (id.as_str(), i)).collect();
        let mut adjacency = vec![Vec::new(); candidates.len()];
        for (a, b) in &pairs {
            if let (Some(&i), Some(&j)) = (index_of.get(a.as_str()), index_of.get(b.as_str())) {
                // Undirected: the query returns each edge once as `a < b`.
                adjacency[i].push(j);
                adjacency[j].push(i);
            }
        }
        adjacency
    }

    /// Momentum-extrapolate this turn's raw query embedding from the
    /// previous turn's (`TrajectoryConfig`), then record the *raw* current
    /// embedding (not the extrapolated one) for next turn's call -- momentum
    /// should track the conversation's actual trajectory, not compound its
    /// own predictions. A zero-length query (no `user_message` this turn)
    /// is returned unchanged and does not touch the stored trajectory
    /// state, so a probe/preflight call with an empty message can't clobber
    /// what a real turn already recorded. Disabled (`cfg.trajectory.enabled
    /// == false`) behaves identically to before this existed: no state
    /// read or written, `query` returned as-is.
    fn trajectory_predict(&self, session_id: &str, query: &[f32]) -> Vec<f32> {
        if !self.cfg.trajectory.enabled || query.is_empty() {
            return query.to_vec();
        }
        let predicted = self
            .trajectory_embeddings
            .get(session_id)
            .map(|prev| ops::extrapolate(query, prev.value(), self.cfg.trajectory.momentum))
            .unwrap_or_else(|| query.to_vec());
        self.trajectory_embeddings.insert(session_id.to_string(), query.to_vec());
        predicted
    }

    /// Drop this session's stored trajectory embedding. Called from the
    /// background idle sweep once a session is being consolidated/closed
    /// out, so `trajectory_embeddings` doesn't grow unbounded over the
    /// daemon's lifetime the way `learn_locks` would without
    /// `prune_idle_learn_locks` — same class of fix, smaller map.
    pub fn forget_trajectory(&self, session_id: &str) {
        self.trajectory_embeddings.remove(session_id);
    }

    /// `[Worth testing this turn]`: the oldest-flagged assumption that's
    /// already crossed into `Surfaced` (the state machine promotes
    /// `Scheduled` -> `Surfaced` at `SCHEDULE_AFTER_EXCHANGES` elapsed,
    /// advanced every background tick regardless of maintenance's nightly
    /// gate -- see `maintenance::run_maintenance`). Renders every turn one
    /// exists, per the plan's "if a scheduled assumption exists" framing --
    /// no additional cadence gate on top.
    async fn worth_testing_line(&self) -> Option<String> {
        let live = self
            .db
            .list_assumptions(Some(crate::store::assumptions::AssumptionState::Surfaced))
            .await
            .ok()?;
        let a = live.into_iter().min_by_key(|a| a.flagged_at_exchange)?;
        let current = self.db.global_exchange_count().await.unwrap_or(0);
        let elapsed = (current - a.flagged_at_exchange).max(0);
        Some(crate::belief::lifecycle::test_prompt(&a.text, elapsed))
    }

    /// `[Where I'm unsure]`: cadence-gated every 12 exchanges (this
    /// session's own counter, not the global one -- the cadence is about
    /// how often *this conversation* sees a doubt line, not overall system
    /// activity). Picks the belief with the highest `confidence · (1 −
    /// support/(support+4))` -- high stated confidence, thin support --
    /// among beliefs not already selected into `[Working assumptions about you]`.
    async fn unsure_line(&self, session_id: &str, selected: &[crate::belief::SelectedBelief]) -> Option<String> {
        let exchange_count = self
            .db
            .get_state(session_id)
            .await
            .ok()
            .flatten()
            .map(|s| s.exchange_count)
            .unwrap_or(0);
        if !crate::antisycophancy::unsure_due(exchange_count) {
            return None;
        }
        let selected_ids: std::collections::HashSet<&str> =
            selected.iter().map(|s| s.belief.id.as_str()).collect();
        let all = self.db.list_beliefs(None).await.ok()?;
        let mut best: Option<(f64, &str)> = None;
        for b in &all {
            if selected_ids.contains(b.id.as_str()) {
                continue;
            }
            let score = b.confidence * (1.0 - (b.support_count as f64) / (b.support_count as f64 + 4.0));
            if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
                best = Some((score, b.text.as_str()));
            }
        }
        best.map(|(_, text)| format!("I think {text}, but I'm not fully confident about it."))
    }

    /// `[Check yourself]`: fires on a detected plateau, gated by a 14-day
    /// dismissal cooldown. No reply-shape-tracking subsystem exists (the
    /// plan's original design), so this uses a cheap, honest proxy actually
    /// available here: `top_belief_id` repeating as the single top-ranked
    /// recall selection across many consecutive turns *is* a form of
    /// plateau -- the system telling the user the same single thing about
    /// themselves over and over with no new signal. Streak state lives in
    /// `app_settings` (global, not session-scoped -- a real simplification,
    /// but this section is explicitly the rarest-firing of the four).
    async fn check_yourself_line(&self, top_belief_id: &str) -> Option<String> {
        const STREAK_ID_KEY: &str = "top_belief_repeat_id";
        const STREAK_KEY: &str = "top_belief_repeat_streak";
        const LAST_SHOWN_KEY: &str = "check_yourself_last_shown_at";
        const PLATEAU_STREAK: i64 = 20;

        let prev_id = self.db.get_setting(STREAK_ID_KEY).await.ok().flatten();
        let prev_streak: i64 = self
            .db
            .get_setting(STREAK_KEY)
            .await
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let streak = if prev_id.as_deref() == Some(top_belief_id) {
            prev_streak + 1
        } else {
            1
        };
        let _ = self.db.set_setting(STREAK_ID_KEY, top_belief_id).await;
        let _ = self.db.set_setting(STREAK_KEY, &streak.to_string()).await;

        let had_plateau = streak >= PLATEAU_STREAK;
        let last_shown_days_ago = self
            .db
            .get_setting(LAST_SHOWN_KEY)
            .await
            .ok()
            .flatten()
            .and_then(|v| v.parse::<i64>().ok())
            .and_then(|ts| chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0))
            .map(|t| (chrono::Utc::now() - t).num_days())
            .unwrap_or(i64::MAX);

        let line = crate::antisycophancy::check_yourself(had_plateau, last_shown_days_ago)?;
        let _ = self
            .db
            .set_setting(LAST_SHOWN_KEY, &chrono::Utc::now().timestamp().to_string())
            .await;
        // Reset the streak once shown, so it doesn't refire every turn
        // until the cooldown elapses again.
        let _ = self.db.set_setting(STREAK_KEY, "0").await;
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn trajectory_predict_first_turn_returns_query_unchanged() {
        // No prior state for this session -- must behave exactly like
        // before trajectory extrapolation existed (regression guard).
        let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
        let q = vec![0.6_f32, 0.8];
        assert_eq!(engine.trajectory_predict("s1", &q), q);
    }

    #[tokio::test]
    async fn trajectory_predict_disabled_never_extrapolates_or_records_state() {
        let mut cfg = Config::default();
        cfg.trajectory.enabled = false;
        let engine = PathwayEngine::open_in_memory(cfg).await.unwrap();
        let a = vec![1.0_f32, 0.0];
        let b = vec![0.0_f32, 1.0];
        assert_eq!(engine.trajectory_predict("s1", &a), a);
        // If state had been recorded from the first call despite being
        // disabled, this second call would extrapolate away from `b`;
        // it must instead return `b` unchanged.
        assert_eq!(engine.trajectory_predict("s1", &b), b);
    }

    #[tokio::test]
    async fn trajectory_predict_extrapolates_from_the_previous_turn() {
        let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
        let a = vec![1.0_f32, 0.0];
        let b = vec![0.0_f32, 1.0];
        assert_eq!(engine.trajectory_predict("s1", &a), a, "first turn has no history yet");

        let momentum = engine.cfg.trajectory.momentum;
        let predicted = engine.trajectory_predict("s1", &b);
        assert_eq!(predicted, ops::extrapolate(&b, &a, momentum));
        // Must actually differ from the raw current-turn embedding --
        // otherwise this is a no-op wearing a trajectory costume.
        assert_ne!(predicted, b);
    }

    #[tokio::test]
    async fn trajectory_predict_stores_the_raw_embedding_not_the_prediction() {
        // If momentum compounded its own predictions instead of tracking
        // the actual trajectory, a third call's prediction would diverge
        // from a fresh two-call sequence ending at the same points.
        let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
        let a = vec![1.0_f32, 0.0];
        let b = vec![0.0_f32, 1.0];
        let c = vec![-1.0_f32, 0.0];
        engine.trajectory_predict("s1", &a);
        engine.trajectory_predict("s1", &b);
        let predicted_c = engine.trajectory_predict("s1", &c);

        let fresh = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
        fresh.trajectory_predict("s2", &b); // seed history directly at b, skipping a
        let fresh_predicted_c = fresh.trajectory_predict("s2", &c);
        assert_eq!(predicted_c, fresh_predicted_c, "prediction must depend only on the last raw turn, not a compounded history");
    }

    #[tokio::test]
    async fn trajectory_predict_empty_query_does_not_touch_stored_state() {
        let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
        let a = vec![1.0_f32, 0.0];
        engine.trajectory_predict("s1", &a);

        // An empty query (e.g. a preflight call with no user_message) must
        // be a pure pass-through that never clobbers the recorded turn.
        assert_eq!(engine.trajectory_predict("s1", &[]), Vec::<f32>::new());

        let b = vec![0.0_f32, 1.0];
        let momentum = engine.cfg.trajectory.momentum;
        let predicted = engine.trajectory_predict("s1", &b);
        assert_eq!(predicted, ops::extrapolate(&b, &a, momentum), "the empty call must not have overwritten `a`");
    }

    #[tokio::test]
    async fn forget_trajectory_clears_a_sessions_stored_embedding() {
        let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
        let a = vec![1.0_f32, 0.0];
        engine.trajectory_predict("s1", &a);
        engine.forget_trajectory("s1");
        // With history cleared, the next call behaves like a first turn.
        let b = vec![0.0_f32, 1.0];
        assert_eq!(engine.trajectory_predict("s1", &b), b);
    }

    #[tokio::test]
    async fn recall_with_trajectory_enabled_across_multiple_turns_does_not_crash() {
        // End-to-end smoke test through the public `recall` entry point --
        // real text goes through the hashing embedding fallback (no live
        // Ollama in tests), whose exact output direction isn't something a
        // test can hand-pick, so this only asserts the multi-turn path
        // stays well-formed, mirroring `domain_routing_prefers_in_domain_beliefs`
        // in `tests/recall_engine.rs` for the same reason.
        let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
        engine
            .db
            .insert_belief(&crate::store::beliefs::Belief {
                id: "b1".into(),
                text: "The user writes Rust for a living.".into(),
                embedding: vec![1.0, 0.0],
                confidence: 0.7,
                provenance: crate::store::beliefs::Provenance::DirectStatement,
                layer: crate::store::beliefs::Layer::Context,
                tested: true,
                domain: None,
                tier: "context".into(),
                support_count: 1,
                distinct_sessions: 1,
                contradict_count: 0,
                pinned: false,
                last_confirmed_at: Some(chrono::Utc::now()),
                consolidated_at: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                session_id: None,
                embedding_model: crate::config::DEFAULT_EMBEDDING_MODEL.into(),
            })
            .await
            .unwrap();
        for msg in ["tell me about my project", "what else do you know"] {
            let block = engine.recall("s1", msg).await;
            assert!(block.is_some());
        }
    }

    #[tokio::test]
    async fn prune_idle_learn_locks_drops_unheld_entries_only() {
        let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();

        // Acquire and immediately release a lock for s1 -- no clone stays
        // alive past the `learn_lock` call, so it's prunable.
        {
            let _guard = engine.learn_lock("s1").await.unwrap();
        }
        assert_eq!(engine.learn_locks_len(), 1);

        // s2's lock is held for the duration of this block -- the DashMap
        // entry plus this held clone means strong_count == 2, not prunable.
        let held = engine.learn_lock("s2").await.unwrap();

        engine.prune_idle_learn_locks();
        assert_eq!(engine.learn_locks_len(), 1, "only s1's unheld lock should be pruned");

        drop(held);
        engine.prune_idle_learn_locks();
        assert_eq!(engine.learn_locks_len(), 0, "s2's lock becomes prunable once released");
    }
}
