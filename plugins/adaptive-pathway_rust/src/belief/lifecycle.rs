//! The belief state machine for scheduled assumptions (a belief below the
//! surfaced/tested boundary slated for testing).

use crate::store::assumptions::{Assumption, AssumptionState};
use crate::store::Db;
use crate::error::Result;

impl Db {
    /// Advance an assumption's state, mirroring the plan:
    /// flag at confidence ≥ 0.55 AND !tested → scheduled; +20 exchanges →
    /// surfaced on render; passed/failed on next direct touch; stale after
    /// 60 unresolved exchanges.
    pub async fn advance_assumption_states(&self, now_exchange: i64) -> Result<()> {
        for a in self.list_assumptions(None).await? {
            let next = next_state(&a, now_exchange);
            if next != a.state {
                self.update_assumption_state(&a.id, next, a.exchanged_since_flag + 1).await?;
            }
        }
        Ok(())
    }
}

fn next_state(a: &Assumption, now: i64) -> AssumptionState {
    match a.state {
        AssumptionState::Scheduled | AssumptionState::Surfaced => {
            if a.exchanged_since_flag >= 60 {
                AssumptionState::Stale
            } else if a.exchanged_since_flag >= 20 {
                AssumptionState::Surfaced
            } else {
                a.state
            }
        }
        _ => a.state,
    }
}

/// Flag an assumption for testing: confidence ≥ 0.55 and not yet tested.
pub fn should_flag(confidence: f64, tested: bool) -> bool {
    confidence >= 0.55 && !tested
}
