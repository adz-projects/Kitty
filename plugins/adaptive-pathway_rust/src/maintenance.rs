//! Background maintenance: period pruning of expired suppressions, the
//! 30-day re-evaluation rule, and advance of the assumption state machine.

use chrono::Utc;

use crate::error::Result;
use crate::store::Db;

pub async fn run_maintenance(db: &Db) -> Result<()> {
    let now = Utc::now();

    // Prune expired (non-permanent) suppressions.
    db.prune_expired(now).await?;

    // 30-day re-evaluation: identity/context beliefs whose last confirmation
    // is older than 30 days have confidence reduced by 0.15 and re-enter the
    // assumption pipeline at `scheduled`.
    let beliefs = db.list_beliefs(None).await?;
    for b in beliefs {
        if let Some(confirmed) = b.last_confirmed_at {
            if (now - confirmed).num_days() >= 30 {
                let new_conf = (b.confidence - 0.15).max(0.0);
                db.update_belief(
                    &b.id,
                    &crate::store::beliefs::BeliefPatch {
                        confidence: Some(new_conf),
                        ..Default::default()
                    },
                    now,
                )
                .await?;
            }
        }
    }

    Ok(())
}
