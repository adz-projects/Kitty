//! Background maintenance: cheap per-tick pruning plus a nightly (~24h
//! gated) pass that applies the 30-day preference re-evaluation rule.
//!
//! `run_maintenance` is called from every background sweep tick (currently
//! every 60s, see `background.rs`), but the expensive belief-decay pass must
//! not run at tick cadence -- doing so would zero out every belief's
//! confidence within minutes instead of over the intended ~30-day windows.
//! It self-gates against a persisted `app_settings['last_maintenance_at']`
//! timestamp so the nightly cadence survives daemon restarts without needing
//! an in-memory tick counter.

use chrono::Utc;

use crate::error::Result;
use crate::store::assumptions::{Assumption, AssumptionState};
use crate::store::Db;

const LAST_MAINTENANCE_KEY: &str = "last_maintenance_at";
const MAINTENANCE_INTERVAL_HOURS: i64 = 24;

/// Confidence penalty applied to a tested belief that has gone 30+ days
/// without reconfirmation (the plan's "preference re-evaluation" rule).
const STALE_CONFIDENCE_PENALTY: f64 = 0.15;
const STALE_AFTER_DAYS: i64 = 30;

pub async fn run_maintenance(db: &Db) -> Result<()> {
    let now = Utc::now();

    // Cheap, always run regardless of cadence: prune expired (non-permanent)
    // suppressions (a single bounded DELETE), and advance the assumption
    // state machine. Both are pure arithmetic + small row-count updates, no
    // LLM call, so there's no reason to defer them to the nightly gate the
    // way the belief-decay pass below needs -- and deferring the schedule/
    // stale transitions would mean "worth testing this turn" only updates
    // once a day, undermining the whole point of exchange-scale scheduling.
    db.prune_expired(now).await?;
    db.advance_assumption_states(db.global_exchange_count().await?).await?;

    if !due(db, now).await? {
        return Ok(());
    }

    reevaluate_stale_beliefs(db, now).await?;

    db.set_setting(LAST_MAINTENANCE_KEY, &now.timestamp().to_string()).await?;
    db.audit("maintenance", None).await.ok();
    Ok(())
}

async fn due(db: &Db, now: chrono::DateTime<Utc>) -> Result<bool> {
    let last = db.get_setting(LAST_MAINTENANCE_KEY).await?;
    Ok(match last.and_then(|raw| raw.parse::<i64>().ok()) {
        Some(ts) => match chrono::DateTime::<Utc>::from_timestamp(ts, 0) {
            Some(last_run) => (now - last_run).num_hours() >= MAINTENANCE_INTERVAL_HOURS,
            None => true,
        },
        None => true,
    })
}

/// 30-day re-evaluation: **tested** beliefs (the plan scopes this to
/// confirmed preferences going stale, not unconfirmed ones already discounted
/// by the untested ceiling) whose `last_confirmed_at` is 30+ days old have
/// confidence reduced by `STALE_CONFIDENCE_PENALTY` and re-enter the
/// assumption pipeline at `scheduled`.
///
/// Bumping `last_confirmed_at` to `now` when the penalty is applied marks the
/// reconsideration as having happened, so this does not refire on every
/// nightly pass until the belief is either reconfirmed by new evidence or
/// another 30 days pass. It also intentionally refreshes the recency anchor
/// `effective_weight` reads from -- unlike an ordinary confidence write, a
/// re-evaluation genuinely *is* a reconfirmation event, so resetting the
/// decay clock here is correct rather than a caching artifact.
async fn reevaluate_stale_beliefs(db: &Db, now: chrono::DateTime<Utc>) -> Result<()> {
    let current_exchange = db.global_exchange_count().await?;
    let beliefs = db.list_beliefs(None).await?;
    for b in beliefs {
        if !b.tested {
            continue;
        }
        let Some(confirmed) = b.last_confirmed_at else { continue };
        if (now - confirmed).num_days() < STALE_AFTER_DAYS {
            continue;
        }

        let new_conf = (b.confidence - STALE_CONFIDENCE_PENALTY).max(0.0);
        db.update_belief(
            &b.id,
            &crate::store::beliefs::BeliefPatch {
                confidence: Some(new_conf),
                last_confirmed_at: Some(now),
                ..Default::default()
            },
            now,
        )
        .await?;

        // Re-enter the assumption pipeline at `scheduled`, unless a row is
        // already tracking this belief (avoid duplicate rows piling up
        // across nightly passes before the existing one resolves). This is
        // a distinct entry path from `flag_assumption_if_warranted`'s
        // confidence-based untested-pattern gate -- a *tested* belief going
        // stale is a different reason to schedule a retest, not "an
        // inference that was never validated".
        if db.get_assumption_for_belief(&b.id).await?.is_none() {
            db.insert_assumption(&Assumption {
                id: crate::store::audit::uuid_string(),
                belief_id: Some(b.id.clone()),
                text: b.text.clone(),
                confidence: new_conf,
                state: AssumptionState::Scheduled,
                flagged_at_exchange: current_exchange,
                created_at: now,
                updated_at: now,
            })
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::beliefs::{Belief, Layer, Provenance};

    fn tested_belief(id: &str, confidence: f64, confirmed_days_ago: i64) -> Belief {
        let now = Utc::now();
        Belief {
            id: id.into(),
            text: format!("belief {id}"),
            embedding: vec![1.0, 0.0],
            confidence,
            provenance: Provenance::DirectStatement,
            layer: Layer::Context,
            tested: true,
            domain: None,
            tier: "context".into(),
            support_count: 3,
            distinct_sessions: 2,
            contradict_count: 0,
            pinned: false,
            last_confirmed_at: Some(now - chrono::Duration::days(confirmed_days_ago)),
            consolidated_at: None,
            created_at: now - chrono::Duration::days(confirmed_days_ago),
            updated_at: now - chrono::Duration::days(confirmed_days_ago),
            session_id: None,
        }
    }

    #[tokio::test]
    async fn first_run_is_always_due_and_decays_only_stale_tested_beliefs() {
        let db = Db::open_in_memory().await.unwrap();
        let fresh = tested_belief("fresh", 0.70, 5);
        let stale = tested_belief("stale", 0.70, 31);
        db.insert_belief(&fresh).await.unwrap();
        db.insert_belief(&stale).await.unwrap();

        run_maintenance(&db).await.unwrap();

        let fresh_after = db.get_belief("fresh").await.unwrap().unwrap();
        let stale_after = db.get_belief("stale").await.unwrap().unwrap();
        assert!((fresh_after.confidence - 0.70).abs() < 1e-9, "fresh belief must not decay");
        assert!((stale_after.confidence - 0.55).abs() < 1e-9, "stale belief decays by 0.15");
        assert!(db.get_assumption_for_belief("stale").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn second_run_within_24h_does_not_redecay() {
        let db = Db::open_in_memory().await.unwrap();
        let stale = tested_belief("stale", 0.70, 31);
        db.insert_belief(&stale).await.unwrap();

        run_maintenance(&db).await.unwrap();
        // Immediately run again -- must be a no-op for the decay pass since
        // less than 24h has elapsed since the first run.
        run_maintenance(&db).await.unwrap();

        let after = db.get_belief("stale").await.unwrap().unwrap();
        assert!((after.confidence - 0.55).abs() < 1e-9, "must not decay twice within 24h");
    }

    #[tokio::test]
    async fn untested_beliefs_are_never_touched_by_reevaluation() {
        let db = Db::open_in_memory().await.unwrap();
        let mut untested = tested_belief("untested", 0.70, 31);
        untested.tested = false;
        db.insert_belief(&untested).await.unwrap();

        run_maintenance(&db).await.unwrap();

        let after = db.get_belief("untested").await.unwrap().unwrap();
        assert!((after.confidence - 0.70).abs() < 1e-9);
        assert!(db.get_assumption_for_belief("untested").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn prune_expired_runs_every_call_regardless_of_gate() {
        // Even when the nightly gate isn't due, expired suppressions must
        // still be pruned every call -- it's cheap and unconditional.
        let db = Db::open_in_memory().await.unwrap();
        db.insert_suppression(&crate::store::suppressions::Suppression {
            id: "s1".into(),
            belief_id: None,
            text_hash: "h1".into(),
            reason: crate::store::suppressions::SuppressReason::Outdated,
            permanent: false,
            expires_at: Some(Utc::now() - chrono::Duration::days(1)),
            created_at: Utc::now() - chrono::Duration::days(91),
        })
        .await
        .unwrap();

        run_maintenance(&db).await.unwrap();
        run_maintenance(&db).await.unwrap();

        assert!(!db.is_text_suppressed("h1", Utc::now()).await.unwrap());
    }
}
