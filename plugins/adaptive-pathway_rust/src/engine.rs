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

/// One turn's recall, as [`PathwayEngine::recall_turn`] returns it.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnRecall {
    /// The rendered block (system block or thought seed).
    pub text: String,
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
    /// The judge's last verdict per session, keyed by a hash of the message
    /// it judged, so re-running the same turn (a regenerate, a retry)
    /// reuses it instead of asking again and possibly hearing something
    /// different. One entry per session; only the latest message matters.
    judge_cache: DashMap<String, (u64, Option<crate::recall::Relevance>)>,
    /// The host's optional judge (`traits::MemoryGate`), set once after open.
    gate: std::sync::OnceLock<Arc<dyn crate::traits::MemoryGate>>,
}

/// How many top candidates the gate is asked to judge for relevance. The
/// rest keep their score; asking about all of a large graph would spend the
/// recall budget on beliefs DPP would almost never pick anyway.
const RELEVANCE_POOL: usize = 32;

impl PathwayEngine {
    /// Hand the engine a `MemoryGate`. Once; later calls are ignored.
    pub fn set_gate(&self, gate: Arc<dyn crate::traits::MemoryGate>) {
        let _ = self.gate.set(gate);
    }

    pub fn gate(&self) -> Option<&Arc<dyn crate::traits::MemoryGate>> {
        self.gate.get()
    }

    pub async fn open(path: &str, cfg: Config) -> Result<Arc<Self>> {
        Self::open_with_embedder(path, cfg, None).await
    }

    /// Open with a host-supplied in-process semantic embedder (BigTiny hands
    /// in its own llama.cpp embedder here; see
    /// [`crate::embed::SemanticEmbedder`]). `None` is [`Self::open`].
    ///
    /// **`cfg.embedding.ollama_model` is the vector space's identity**, not
    /// just a label: `list_recall_candidates` filters on it, and
    /// `sync_embedding_model_fingerprint` below compares it against what's on
    /// disk. A host swapping the embedder MUST also change that string, or
    /// vectors from two different models silently share one pool. Changing it
    /// marks existing beliefs stale, which `background::reembed_stale_beliefs`
    /// then migrates.
    pub async fn open_with_embedder(
        path: &str,
        cfg: Config,
        embedder: Option<Arc<dyn crate::embed::SemanticEmbedder>>,
    ) -> Result<Arc<Self>> {
        let db = Db::open(path).await?;
        if db.sync_embedding_model_fingerprint(&cfg.embedding.ollama_model).await.unwrap_or(false) {
            tracing::info!(
                model = %cfg.embedding.ollama_model,
                "embedding model changed (or first run) -- stale beliefs will be re-embedded in the background"
            );
        }
        let embed = EmbeddingProvider::with_embedder(cfg.clone(), embedder);
        Ok(Arc::new(Self {
            db,
            cfg,
            embed,
            paused_override: DashMap::new(),
            learn_locks: DashMap::new(),
            chat_slot: Arc::new(Semaphore::new(1)),
            judge_cache: DashMap::new(),
            gate: std::sync::OnceLock::new(),
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
            judge_cache: DashMap::new(),
            gate: std::sync::OnceLock::new(),
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
        self.recall_turn(session_id, user_message, None, false).await.map(|t| t.text)
    }

    /// The recall for one turn: the rendered block, as a labelled system
    /// block or (with `seeded`) as a `<think>` thought seed, plus what the
    /// host needs alongside it.
    ///
    /// **Deterministic.** The block is a function of the stored state and of
    /// `user_message` / `previous_user_message` only: no in-memory history,
    /// and nothing recall writes changes what a later recall of the same
    /// message renders until the turn has ended (`record_turn_end`) or the
    /// nightly pass has run. The host passes the chat's previous user
    /// message, which drives the trajectory extrapolation the in-memory
    /// embedding map used to.
    pub async fn recall_turn(
        &self,
        session_id: &str,
        user_message: &str,
        previous_user_message: Option<&str>,
        seeded: bool,
    ) -> Option<TurnRecall> {
        let (mut selected, candidates) = self
            .select_for_turn(session_id, user_message, previous_user_message)
            .await?;
        let signals = self.turn_signals(session_id, &selected, &candidates).await;
        let text = if seeded {
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
            crate::recall::cap_reflection_to_token_budget(block)
        } else {
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
            crate::recall::cap_to_token_budget(block)
        };
        Some(TurnRecall { text })
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
        self.recall_turn(session_id, user_message, None, true).await.map(|t| t.text)
    }

    /// The three anti-sycophancy signals both render paths carry, gathered
    /// once so `recall` and `recall_thought_seed` cannot diverge on which
    /// ones they surface. Cadence/eligibility gating lives in the individual
    /// `*_line` helpers, not here.
    async fn turn_signals(
        &self,
        session_id: &str,
        selected: &[crate::belief::SelectedBelief],
        candidates: &[crate::store::beliefs::Belief],
    ) -> TurnSignals {
        // Display order's first belief (weight desc, id asc) -- the same one
        // `select_for_turn` records first in `last_recall_ids`, which
        // `record_turn_end` advances the streak from.
        let top_belief_id = selected
            .iter()
            .min_by(|a, b| {
                b.effective_weight
                    .partial_cmp(&a.effective_weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.belief.id.cmp(&b.belief.id))
            })
            .map(|s| s.belief.id.clone());
        TurnSignals {
            worth_testing: self.worth_testing_line().await,
            unsure: self.unsure_line(session_id, selected, candidates).await,
            check: match &top_belief_id {
                Some(id) => self.check_yourself_line(session_id, id).await,
                None => None,
            },
        }
    }

    /// Shared candidate-selection pass behind `recall`/`recall_thought_seed`:
    /// pause gate, embedding-model-scoped candidates, suppression filter,
    /// trajectory-predicted query, domain inference, diffusion+DPP
    /// selection. `None` on any of: paused, no candidates, everything
    /// suppressed, or DPP selecting nothing.
    ///
    /// Also returns the filtered candidate pool (current embedding space,
    /// suppression applied, other sessions' conversation beliefs excluded)
    /// so the per-turn signals draw from the same eligible set as the main
    /// block -- `[Where I'm unsure]` used to read the whole table and could
    /// resurface a forgotten or retired belief.
    async fn select_for_turn(
        &self,
        session_id: &str,
        user_message: &str,
        previous_user_message: Option<&str>,
    ) -> Option<(Vec<crate::belief::SelectedBelief>, Vec<crate::store::beliefs::Belief>)> {
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

        let (query, semantic): (Vec<f32>, bool) = if user_message.trim().is_empty() {
            (Vec::new(), false)
        } else {
            match self.embed.embed_with_space(user_message).await {
                // Every candidate lives in the semantic space (filtered on
                // `ollama_model` above). A lexical hash-fallback query
                // cosine-compared against them is meaningless and would drive
                // the relevance floor with noise, so a fallback turn selects
                // on weight alone -- exactly the documented "embedder down"
                // degradation.
                (v, true) => (v, true),
                (_, false) => (Vec::new(), false),
            }
        };
        // Momentum-extrapolate toward where the conversation is heading
        // (`TrajectoryConfig`) before using the query for anything --
        // `predicted_query` is what actually drives domain inference and
        // belief ranking, not the raw current-turn embedding. The previous
        // query is the chat's previous user message, embedded again (the
        // embedding cache usually has it from last turn), never in-memory
        // history: the same message after the same history extrapolates the
        // same way, across restarts and regenerates.
        let previous_query: Vec<f32> = match previous_user_message.map(str::trim) {
            Some(prev) if semantic && !prev.is_empty() && self.cfg.trajectory.enabled => {
                match self.embed.embed_with_space(prev).await {
                    (v, true) => v,
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        };
        let predicted_query = trajectory_predict(&query, &previous_query, &self.cfg.trajectory);
        let query_domain = crate::domains::infer_query_domain(&candidates, &predicted_query);
        let cooccurrence = self.cooccurrence_adjacency(&candidates).await;
        let relevance = self
            .judge_relevance(session_id, user_message, &candidates, &predicted_query, query_domain.as_deref())
            .await;

        // Self-tuning floor: raise the configured `min_cosine` toward the
        // snapshot of where relevance usually cuts in this embedding space
        // (minus slack). The snapshot only moves in the nightly pass
        // (`recall::cut_log`), so it cannot shift between two recalls of the
        // same turn. Semantic query only; cold leaves the hard floor.
        let space = self.cfg.embedding.ollama_model.clone();
        let mut turn_cfg = self.cfg.clone();
        if semantic {
            if let Some(ewma) = crate::recall::cut_log::snapshot(&self.db, &space).await {
                turn_cfg.recall.min_cosine =
                    self.cfg.recall.min_cosine.max(ewma - self.cfg.recall.ewma_slack);
            }
        }

        let (selected, cut) = crate::recall::select_beliefs_relevant_with_cut(
            &candidates,
            &predicted_query,
            query_domain.as_deref(),
            &cooccurrence,
            relevance.as_ref(),
            &turn_cfg,
        );

        // Log this turn's effective cut for tonight's fold (semantic turns
        // only). Logging does not change the snapshot recall reads.
        // Best-effort -- a settings write must never fail recall.
        if semantic {
            if let Some(cut) = cut {
                if let Err(e) = crate::recall::cut_log::log(&self.db, &space, cut).await {
                    tracing::debug!("pathway: could not log the recall cut: {e}");
                }
            }
        }
        // Recorded, in display order (weight desc, id asc: what
        // `render_knows` prints), so what recall injected can be read from
        // disk, and `record_turn_end` knows this turn's top belief.
        // Best-effort.
        let mut ordered: Vec<&crate::belief::SelectedBelief> = selected.iter().collect();
        ordered.sort_by(|a, b| {
            b.effective_weight
                .partial_cmp(&a.effective_weight)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.belief.id.cmp(&b.belief.id))
        });
        let ids: Vec<String> = ordered.iter().map(|s| s.belief.id.clone()).collect();
        if let Err(e) = self.db.set_last_recall_ids(session_id, &ids).await {
            tracing::debug!("pathway: could not record recall ids for {session_id}: {e}");
        }
        if selected.is_empty() {
            None
        } else {
            Some((selected, candidates))
        }
    }

    /// Ask the gate, if there is one, how relevant the strongest candidates are
    /// to this message. `None` (no gate, no opinion, nothing to ask about)
    /// leaves selection exactly as it was.
    async fn judge_relevance(
        &self,
        session_id: &str,
        user_message: &str,
        candidates: &[crate::store::beliefs::Belief],
        query: &[f32],
        query_domain: Option<&str>,
    ) -> Option<crate::recall::Relevance> {
        let gate = self.gate()?;
        if user_message.trim().is_empty() {
            return None;
        }
        // Same message as this session's last judged one: reuse the verdict.
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            user_message.hash(&mut h);
            h.finish()
        };
        if let Some(hit) = self.judge_cache.get(session_id) {
            if hit.0 == key {
                return hit.1.clone();
            }
        }
        let verdict = self.judge_relevance_uncached(user_message, candidates, query, query_domain, gate).await;
        self.judge_cache.insert(session_id.to_string(), (key, verdict.clone()));
        verdict
    }

    async fn judge_relevance_uncached(
        &self,
        user_message: &str,
        candidates: &[crate::store::beliefs::Belief],
        query: &[f32],
        query_domain: Option<&str>,
        gate: &Arc<dyn crate::traits::MemoryGate>,
    ) -> Option<crate::recall::Relevance> {
        let pool = crate::recall::preselect(candidates, query, query_domain, RELEVANCE_POOL);
        if pool.is_empty() {
            return None;
        }
        let texts: Vec<&str> = pool.iter().map(|&i| candidates[i].text.as_str()).collect();
        let (ps, drop_below) = gate.belief_relevance(user_message, &texts).await?;
        let by_index = pool
            .into_iter()
            .zip(ps)
            .filter_map(|(i, p)| p.map(|p| (i, p)))
            .collect();
        Some(crate::recall::Relevance { by_index, drop_below })
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
    /// among this turn's eligible candidates (see `select_for_turn`) not
    /// already selected into `[Working assumptions about you]`.
    async fn unsure_line(
        &self,
        session_id: &str,
        selected: &[crate::belief::SelectedBelief],
        candidates: &[crate::store::beliefs::Belief],
    ) -> Option<String> {
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
        let mut best: Option<(f64, &str)> = None;
        for b in candidates {
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
    /// cooldown. The plateau proxy: the same belief has been this chat's
    /// top-ranked recall for `PLATEAU_STREAK` turns in a row -- the system
    /// telling the user the same single thing about themselves with no new
    /// signal.
    ///
    /// **Read-only.** Recall used to advance the streak itself, so recalling
    /// the same turn twice moved it, and the streak was global across chats.
    /// Now it is per chat and advanced only by [`Self::record_turn_end`];
    /// recall computes what this turn's streak *would* be and decides from
    /// that, so the same stored state renders the same block.
    async fn check_yourself_line(&self, session_id: &str, top_belief_id: &str) -> Option<String> {
        let (shown, _) = self.check_decision(session_id, top_belief_id, chrono::Utc::now()).await;
        shown.then(|| crate::antisycophancy::check_yourself(true, i64::MAX)).flatten()
    }

    /// `(show this turn, streak this turn)` for `[Check yourself]`, from the
    /// stored per-chat streak and cooldown.
    async fn check_decision(
        &self,
        session_id: &str,
        top_belief_id: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> (bool, i64) {
        let stored = self
            .db
            .get_setting(&format!("{CHECK_STREAK_PREFIX}{session_id}"))
            .await
            .ok()
            .flatten();
        let (prev_id, prev_streak) = stored
            .as_deref()
            .and_then(|v| v.rsplit_once('|'))
            .map(|(id, n)| (id.to_string(), n.parse::<i64>().unwrap_or(0)))
            .unwrap_or_default();
        let streak = if prev_id == top_belief_id { prev_streak + 1 } else { 1 };
        let last_shown_days_ago = self
            .db
            .get_setting(&format!("{CHECK_SHOWN_PREFIX}{session_id}"))
            .await
            .ok()
            .flatten()
            .and_then(|v| v.parse::<i64>().ok())
            .and_then(|ts| chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0))
            .map(|t| (now - t).num_days())
            .unwrap_or(i64::MAX);
        let shown = crate::antisycophancy::check_yourself(streak >= PLATEAU_STREAK, last_shown_days_ago).is_some();
        (shown, streak)
    }

    /// Turn-end bookkeeping the deterministic recall leaves to the host:
    /// advance this chat's `[Check yourself]` streak from the turn's top
    /// recalled belief (`last_recall_ids`, display order), and start the
    /// cooldown when the turn showed the line. Call once per completed turn,
    /// after the reply. Best-effort.
    pub async fn record_turn_end(&self, session_id: &str) {
        let Some(top) = self
            .db
            .get_state(session_id)
            .await
            .ok()
            .flatten()
            .and_then(|s| s.last_recall_ids.into_iter().next())
        else {
            return;
        };
        let now = chrono::Utc::now();
        let (shown, streak) = self.check_decision(session_id, &top, now).await;
        let streak_key = format!("{CHECK_STREAK_PREFIX}{session_id}");
        if shown {
            let _ = self
                .db
                .set_setting(&format!("{CHECK_SHOWN_PREFIX}{session_id}"), &now.timestamp().to_string())
                .await;
            // Reset once shown, so it does not refire until a new plateau.
            let _ = self.db.set_setting(&streak_key, &format!("{top}|0")).await;
        } else {
            let _ = self.db.set_setting(&streak_key, &format!("{top}|{streak}")).await;
        }
    }
}

/// Turns the same top belief must lead recall before `[Check yourself]`.
const PLATEAU_STREAK: i64 = 20;
const CHECK_STREAK_PREFIX: &str = "check_streak::";
const CHECK_SHOWN_PREFIX: &str = "check_shown::";

/// Momentum-extrapolate this turn's query embedding from the previous user
/// message's (`TrajectoryConfig`). A pure function of the two embeddings:
/// no stored state, so the same two messages always extrapolate the same
/// way. An empty current or previous query (no message, no previous turn,
/// embedder down) returns the current query unchanged, as does a disabled
/// trajectory.
pub fn trajectory_predict(query: &[f32], previous: &[f32], cfg: &crate::config::TrajectoryConfig) -> Vec<f32> {
    if !cfg.enabled || query.is_empty() || previous.len() != query.len() {
        return query.to_vec();
    }
    ops::extrapolate(query, previous, cfg.momentum)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trajectory_without_a_previous_message_returns_the_query() {
        let cfg = Config::default().trajectory;
        let q = vec![0.6_f32, 0.8];
        assert_eq!(trajectory_predict(&q, &[], &cfg), q);
    }

    #[test]
    fn a_disabled_trajectory_never_extrapolates() {
        let mut cfg = Config::default().trajectory;
        cfg.enabled = false;
        let (a, b) = (vec![1.0_f32, 0.0], vec![0.0_f32, 1.0]);
        assert_eq!(trajectory_predict(&b, &a, &cfg), b);
    }

    #[test]
    fn trajectory_extrapolates_from_the_previous_message_and_nothing_else() {
        let cfg = Config::default().trajectory;
        let (a, b) = (vec![1.0_f32, 0.0], vec![0.0_f32, 1.0]);
        let predicted = trajectory_predict(&b, &a, &cfg);
        assert_eq!(predicted, ops::extrapolate(&b, &a, cfg.momentum));
        assert_ne!(predicted, b, "must actually extrapolate");
        // Pure: the same inputs give the same output, whatever ran before.
        assert_eq!(trajectory_predict(&b, &a, &cfg), predicted);
    }

    #[tokio::test]
    async fn the_same_turn_recalls_the_same_block_twice() {
        // Recall must be reproducible: same stored state + same messages ->
        // same block (no in-memory trajectory, no floor moving under it).
        let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
        for (i, v) in [vec![1.0_f32, 0.0], vec![0.0_f32, 1.0], vec![0.7_f32, 0.7]].into_iter().enumerate() {
            let mut b = test_belief(&format!("b{i}"), &format!("The user has trait {i}."));
            b.embedding = v;
            engine.db.insert_belief(&b).await.unwrap();
        }
        let first = engine.recall_turn("s1", "what do you know", Some("earlier question"), false).await;
        let second = engine.recall_turn("s1", "what do you know", Some("earlier question"), false).await;
        assert!(first.is_some());
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn the_check_streak_only_moves_at_turn_end() {
        let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
        let mut b = test_belief("top", "The user writes Rust.");
        b.embedding = vec![1.0, 0.0];
        engine.db.insert_belief(&b).await.unwrap();
        for _ in 0..(PLATEAU_STREAK - 1) {
            assert!(!engine.recall("s1", "").await.unwrap().contains("[Check yourself]"));
            engine.record_turn_end("s1").await;
        }
        // Recalling repeatedly without ending the turn does not advance it.
        for _ in 0..3 {
            let _ = engine.recall("s1", "").await;
        }
        let at_plateau = engine.recall("s1", "").await.unwrap();
        assert!(at_plateau.contains("[Check yourself]"), "the 20th turn shows it: {at_plateau}");
        engine.record_turn_end("s1").await;
        assert!(!engine.recall("s1", "").await.unwrap().contains("[Check yourself]"), "then the cooldown holds");
        // Another chat has its own streak.
        assert!(!engine.recall("s2", "").await.unwrap().contains("[Check yourself]"));
    }

    fn test_belief(id: &str, text: &str) -> crate::store::beliefs::Belief {
        crate::store::beliefs::Belief {
            id: id.into(),
            text: text.into(),
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
        }
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
