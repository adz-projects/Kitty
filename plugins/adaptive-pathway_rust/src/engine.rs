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
}

impl PathwayEngine {
    pub async fn open(path: &str, cfg: Config) -> Result<Arc<Self>> {
        let db = Db::open(path).await?;
        let embed = EmbeddingProvider::new(cfg.clone());
        Ok(Arc::new(Self {
            db,
            cfg,
            embed,
            paused_override: DashMap::new(),
            learn_locks: DashMap::new(),
            chat_slot: Arc::new(Semaphore::new(1)),
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
        let embed = EmbeddingProvider::new(cfg.clone());
        Ok(Arc::new(Self {
            db,
            cfg,
            embed,
            paused_override: DashMap::new(),
            learn_locks: DashMap::new(),
            chat_slot: Arc::new(Semaphore::new(1)),
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

    /// Build the full recall block for a turn: `[What I know about you]`
    /// every turn, `[Worth testing this turn]` every turn a live assumption
    /// has surfaced, `[Where I'm unsure]` cadence-gated every 12 exchanges,
    /// `[Check yourself]` on a detected plateau. `None` when there's
    /// nothing to say (paused, disabled, no beliefs) -- zero prompt delta,
    /// cache-preserving; callers must not distinguish `None` from `Some("")`.
    pub async fn recall(&self, session_id: &str, user_message: &str) -> Option<String> {
        if self.is_paused(session_id).await.unwrap_or(false) {
            return None;
        }
        let candidates = self.db.list_recall_candidates(session_id).await.ok()?;
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
        let query_domain = crate::domains::infer_query_domain(&candidates, &query);

        let mut selected =
            crate::recall::select_beliefs_relevant(&candidates, &query, query_domain.as_deref());
        if selected.is_empty() {
            return None;
        }
        let top_belief_id = selected
            .iter()
            .max_by(|a, b| {
                a.effective_weight
                    .partial_cmp(&b.effective_weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|s| s.belief.id.clone());
        let knows = crate::recall::render_knows(&mut selected);

        let worth_testing = self.worth_testing_line().await;
        let unsure = self.unsure_line(session_id, &selected).await;
        let check = match &top_belief_id {
            Some(id) => self.check_yourself_line(id).await,
            None => None,
        };

        let block = crate::antisycophancy::render_block(&knows, worth_testing, unsure, check);
        if block.is_empty() {
            return None;
        }
        Some(crate::recall::cap_to_token_budget(block))
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
    /// among beliefs not already selected into `[What I know about you]`.
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
