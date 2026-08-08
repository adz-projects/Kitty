//! The belief state machine for scheduled assumptions (a belief below the
//! surfaced/tested boundary slated for testing). See `Db::global_exchange_count`
//! for why elapsed exchanges are computed against a fixed anchor rather than
//! a counter re-stamped on every advance call.

use crate::store::assumptions::{Assumption, AssumptionState};
use crate::store::beliefs::Belief;
use crate::store::Db;
use crate::error::Result;

/// Exchanges after which a scheduled/surfaced assumption is considered
/// worth actively surfacing to the model.
pub const SCHEDULE_AFTER_EXCHANGES: i64 = 20;
/// Exchanges after which a still-unresolved assumption is deprioritized.
pub const STALE_AFTER_EXCHANGES: i64 = 60;

impl Db {
    /// Advance every live (scheduled/surfaced) assumption's state against
    /// `current_exchange` (the current global exchange count). Never
    /// touches `flagged_at_exchange` -- only `next_state` reads it, to
    /// compute elapsed exchanges live.
    pub async fn advance_assumption_states(&self, current_exchange: i64) -> Result<()> {
        for a in self.list_live_assumptions().await? {
            let next = next_state(&a, current_exchange);
            if next != a.state {
                self.set_assumption_state(&a.id, next).await?;
            }
        }
        Ok(())
    }

    /// Flag a belief as an assumption worth scheduling for testing, per the
    /// plan: confidence >= 0.55 AND untested AND not already tracked.
    /// Idempotent -- a belief already tracked (in any state) is left alone,
    /// so a repeated observation on the same belief doesn't create
    /// duplicate assumption rows.
    pub async fn flag_assumption_if_warranted(&self, belief: &Belief, current_exchange: i64) -> Result<()> {
        if !should_flag(belief.confidence, belief.tested) {
            return Ok(());
        }
        if self.get_assumption_for_belief(&belief.id).await?.is_some() {
            return Ok(());
        }
        let now = chrono::Utc::now();
        self.insert_assumption(&Assumption {
            id: crate::store::audit::uuid_string(),
            belief_id: Some(belief.id.clone()),
            text: belief.text.clone(),
            confidence: belief.confidence,
            state: AssumptionState::Scheduled,
            flagged_at_exchange: current_exchange,
            created_at: now,
            updated_at: now,
        })
        .await
    }

    /// Resolve any live (scheduled/surfaced) assumption tracking
    /// `belief_id`: `passed` on supporting decisive evidence (the belief
    /// just received `direct_statement`/`correction`/`controlled_test`
    /// provenance), `failed` on a correction/forget. No-op if nothing is
    /// tracking this belief, or it's already resolved.
    pub async fn resolve_assumption_for_belief(&self, belief_id: &str, passed: bool) -> Result<()> {
        let Some(a) = self.get_live_assumption_for_belief(belief_id).await? else {
            return Ok(());
        };
        let next = if passed { AssumptionState::Passed } else { AssumptionState::Failed };
        self.set_assumption_state(&a.id, next).await
    }

    /// Mark an assumption `surfaced` (the recall block actually rendered
    /// its test prompt this turn) -- distinct from `scheduled` so the
    /// staleness clock and the "don't test the same thing every turn"
    /// cadence gate both have a signal to key off.
    pub async fn mark_assumption_surfaced(&self, id: &str) -> Result<()> {
        self.set_assumption_state(id, AssumptionState::Surfaced).await
    }
}

fn next_state(a: &Assumption, current_exchange: i64) -> AssumptionState {
    let elapsed = (current_exchange - a.flagged_at_exchange).max(0);
    match a.state {
        AssumptionState::Scheduled => {
            // Surface BEFORE the stale check: if a single tick jumps a
            // Scheduled assumption past BOTH thresholds (rapid-chat session
            // between two 60s background passes), the old ordering sent it
            // straight to Stale — it was never `Surfaced`, so the
            // `[Worth testing this turn]` prompt never rendered and the
            // "don't test the same thing every turn" cadence had nothing to
            // key off. Surfacing first guarantees the prompt shows at least
            // once; the NEXT tick (state == Surfaced, elapsed still >= 60)
            // then deprioritizes it via the stale branch below.
            if elapsed >= SCHEDULE_AFTER_EXCHANGES {
                AssumptionState::Surfaced
            } else {
                a.state
            }
        }
        AssumptionState::Surfaced => {
            if elapsed >= STALE_AFTER_EXCHANGES {
                AssumptionState::Stale
            } else {
                a.state
            }
        }
        other => other,
    }
}

/// Flag an assumption for testing: confidence >= 0.55 and not yet tested.
pub fn should_flag(confidence: f64, tested: bool) -> bool {
    confidence >= 0.55 && !tested
}

/// Deterministic test-prompt template for `[Worth testing this turn]`,
/// matching the plan's example verbatim in shape.
pub fn test_prompt(text: &str, exchanges_untested: i64) -> String {
    format!(
        "I've assumed \"{text}\" for {exchanges_untested} exchanges without ever checking. \
         Try the opposite here and see whether it lands."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::beliefs::{Layer, Provenance};
    use chrono::Utc;

    fn assumption(state: AssumptionState, flagged_at: i64) -> Assumption {
        let now = Utc::now();
        Assumption {
            id: "a1".into(),
            belief_id: Some("b1".into()),
            text: "user prefers X".into(),
            confidence: 0.6,
            state,
            flagged_at_exchange: flagged_at,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn stays_scheduled_before_the_threshold() {
        let a = assumption(AssumptionState::Scheduled, 100);
        assert_eq!(next_state(&a, 110), AssumptionState::Scheduled);
    }

    #[test]
    fn surfaces_at_twenty_elapsed_exchanges() {
        let a = assumption(AssumptionState::Scheduled, 100);
        assert_eq!(next_state(&a, 120), AssumptionState::Surfaced);
    }

    #[test]
    fn goes_stale_at_sixty_elapsed_exchanges() {
        let a = assumption(AssumptionState::Surfaced, 100);
        assert_eq!(next_state(&a, 160), AssumptionState::Stale);
    }

    #[test]
    fn resolved_states_never_advance() {
        let a = assumption(AssumptionState::Passed, 100);
        assert_eq!(next_state(&a, 999), AssumptionState::Passed);
        let a = assumption(AssumptionState::Failed, 100);
        assert_eq!(next_state(&a, 999), AssumptionState::Failed);
    }

    #[test]
    fn elapsed_is_never_negative_even_if_current_precedes_anchor() {
        // Defensive: if current_exchange somehow regressed (shouldn't
        // happen -- it's monotonic), elapsed clamps to 0 rather than
        // producing a negative "elapsed exchanges" that could satisfy a
        // >= threshold via wraparound-adjacent reasoning.
        let a = assumption(AssumptionState::Scheduled, 100);
        assert_eq!(next_state(&a, 50), AssumptionState::Scheduled);
    }

    #[test]
    fn should_flag_requires_confidence_and_untested() {
        assert!(should_flag(0.55, false));
        assert!(!should_flag(0.54, false));
        assert!(!should_flag(0.80, true));
    }

    #[tokio::test]
    async fn flag_is_idempotent_across_repeated_calls() {
        let db = Db::open_in_memory().await.unwrap();
        let now = Utc::now();
        let belief = Belief {
            id: "b1".into(),
            text: "user prefers concise replies".into(),
            embedding: vec![1.0, 0.0],
            confidence: 0.6,
            provenance: Provenance::InferredPattern,
            layer: Layer::Context,
            tested: false,
            domain: None,
            tier: "context".into(),
            support_count: 1,
            distinct_sessions: 1,
            contradict_count: 0,
            pinned: false,
            last_confirmed_at: Some(now),
            consolidated_at: None,
            created_at: now,
            updated_at: now,
            session_id: None,
            embedding_model: crate::config::DEFAULT_EMBEDDING_MODEL.into(),
        };
        db.insert_belief(&belief).await.unwrap();

        db.flag_assumption_if_warranted(&belief, 10).await.unwrap();
        db.flag_assumption_if_warranted(&belief, 15).await.unwrap();

        let live = db.list_live_assumptions().await.unwrap();
        assert_eq!(live.len(), 1, "repeated flagging must not create duplicate rows");
        assert_eq!(live[0].flagged_at_exchange, 10, "the anchor must not move on a later re-flag attempt");
    }

    #[tokio::test]
    async fn resolve_marks_the_live_assumption_and_ignores_already_resolved() {
        let db = Db::open_in_memory().await.unwrap();
        let now = Utc::now();
        db.insert_belief(&Belief {
            id: "b1".into(),
            text: "x".into(),
            embedding: vec![1.0, 0.0],
            confidence: 0.6,
            provenance: Provenance::InferredPattern,
            layer: Layer::Context,
            tested: false,
            domain: None,
            tier: "context".into(),
            support_count: 1,
            distinct_sessions: 1,
            contradict_count: 0,
            pinned: false,
            last_confirmed_at: Some(now),
            consolidated_at: None,
            created_at: now,
            updated_at: now,
            session_id: None,
            embedding_model: crate::config::DEFAULT_EMBEDDING_MODEL.into(),
        })
        .await
        .unwrap();
        db.insert_assumption(&Assumption {
            id: "a1".into(),
            belief_id: Some("b1".into()),
            text: "x".into(),
            confidence: 0.6,
            state: AssumptionState::Scheduled,
            flagged_at_exchange: 0,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();

        db.resolve_assumption_for_belief("b1", true).await.unwrap();
        let a = db.get_assumption("a1").await.unwrap().unwrap();
        assert_eq!(a.state, AssumptionState::Passed);

        // Resolving again (e.g. a second observation) must not flip an
        // already-resolved assumption.
        db.resolve_assumption_for_belief("b1", false).await.unwrap();
        let a = db.get_assumption("a1").await.unwrap().unwrap();
        assert_eq!(a.state, AssumptionState::Passed, "an already-resolved assumption must not be re-resolved");
    }
}
