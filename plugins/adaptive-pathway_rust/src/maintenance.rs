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

/// Retirement (nightly): a belief this old (by `created_at`, which — unlike
/// `last_confirmed_at` — the stale penalty never bumps, so it is true age) and
/// still dead is suppressed as `Outdated`, i.e. dropped from recall for 90 days
/// but not destroyed; genuine new evidence re-earns it, and the suppression
/// simply re-applies next pass if it stays dead. Longer than the 30-day
/// re-evaluation window so a belief gets a full penalty-and-retest cycle first.
const RETIRE_AFTER_DAYS: i64 = 60;
/// A **tested** belief is dead when stale re-evaluation has driven its
/// confidence to/under this — it went stale and no new evidence brought it
/// back across the cycles it took to decay here. Above the untested-pattern
/// ceiling so a merely-unconfirmed belief isn't caught by the tested branch.
const RETIRE_CONFIDENCE_FLOOR: f64 = 0.35;
/// An **untested** belief (the stale-decay pass deliberately never touches
/// these, so they never reach the confidence floor) is dead when it is this
/// old and still rests on a single uncorroborated observation — an ancient
/// one-off guess. A belief seen in ≥2 observations is corroborated and kept
/// (the relevance gate, not retirement, decides whether it injects).
const RETIRE_MAX_SUPPORT_UNTESTED: i64 = 1;

/// How long routine `consolidate`/`maintenance` audit rows are kept.
const HOUSEKEEPING_AUDIT_DAYS: i64 = 30;

pub async fn run_maintenance(db: &Db) -> Result<()> {
    run_maintenance_with(db, crate::config::Config::default().recall.ewma_alpha).await
}

/// [`run_maintenance`] with the engine's recall-cut smoothing
/// (`recall.ewma_alpha`), which the nightly pass uses to fold the day's
/// logged cuts into the floor snapshot recall reads (`recall::cut_log`).
pub async fn run_maintenance_with(db: &Db, cut_ewma_alpha: f64) -> Result<()> {
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

    // The relevance-floor snapshot moves once a day, here, never during
    // recall (`recall::cut_log`).
    if let Err(e) = crate::recall::cut_log::fold_pending(db, cut_ewma_alpha).await {
        tracing::warn!("pathway: folding recall cuts failed: {e}");
    }
    reevaluate_stale_beliefs(db, now).await?;
    retire_dead_beliefs(db, now).await?;
    if let Err(e) = db.prune_housekeeping_audit(HOUSEKEEPING_AUDIT_DAYS).await {
        tracing::warn!("pathway: audit pruning failed: {e}");
    }

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
        if !b.full_weight() {
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

/// Retirement: drop genuinely-dead beliefs from recall (as `Outdated`, so they
/// are suppressed for 90 days rather than destroyed — reversible, re-earnable).
/// Complements the stale-decay pass: decay lowers a belief's *weight*, but a
/// decayed or ancient belief still sits in the recall candidate pool and, on a
/// small store, still injects. Retirement removes it from the pool.
///
/// Conservative and pinned/identity-exempt (pinned is explicit user intent;
/// identity beliefs are the slowest-moving layer and protected). A belief is
/// retired only when it is old by true age (`created_at`) **and** dead:
/// * **tested** → confidence has decayed to/under `RETIRE_CONFIDENCE_FLOOR`
///   (it went stale and never recovered), or
/// * **untested** → it still rests on a single uncorroborated observation
///   (`support_count ≤ RETIRE_MAX_SUPPORT_UNTESTED`).
///
/// Already-suppressed beliefs are skipped so a nightly re-run doesn't pile up
/// duplicate suppression rows. Best-effort per belief: one failed retirement
/// never aborts the pass.
async fn retire_dead_beliefs(db: &Db, now: chrono::DateTime<Utc>) -> Result<()> {
    for b in db.list_beliefs(None).await? {
        if b.pinned || b.layer == crate::store::beliefs::Layer::Identity {
            continue;
        }
        if (now - b.created_at).num_days() < RETIRE_AFTER_DAYS {
            continue;
        }
        let dead = if b.full_weight() {
            b.confidence <= RETIRE_CONFIDENCE_FLOOR
        } else {
            b.support_count <= RETIRE_MAX_SUPPORT_UNTESTED
        };
        if !dead {
            continue;
        }
        // Don't re-suppress a belief already dropped from recall.
        let text_hash = crate::belief::synthesis::text_hash(&b.text);
        if db.is_text_suppressed(&text_hash, now).await.unwrap_or(false) {
            continue;
        }
        match db.forget_belief_by_id(&b.id, crate::store::suppressions::SuppressReason::Outdated).await {
            Ok(_) => {
                let _ = db.audit("retire", Some(&format!("outdated(90d): {}", b.text))).await;
            }
            Err(e) => tracing::warn!("pathway: retire of belief {} failed: {e}", b.id),
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
            embedding_model: crate::config::DEFAULT_EMBEDDING_MODEL.into(),
            kind: crate::store::beliefs::BeliefKind::Context,
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
        // An inference: a direct statement is re-evaluated like a tested one.
        untested.provenance = Provenance::InferredPattern;
        db.insert_belief(&untested).await.unwrap();

        run_maintenance(&db).await.unwrap();

        let after = db.get_belief("untested").await.unwrap().unwrap();
        assert!((after.confidence - 0.70).abs() < 1e-9);
        assert!(db.get_assumption_for_belief("untested").await.unwrap().is_none());
    }

    #[allow(clippy::too_many_arguments)]
    fn mk(
        id: &str,
        tested: bool,
        confidence: f64,
        created_days_ago: i64,
        support_count: i64,
        pinned: bool,
        layer: Layer,
    ) -> Belief {
        let now = Utc::now();
        let created = now - chrono::Duration::days(created_days_ago);
        Belief {
            id: id.into(),
            text: format!("belief {id}"),
            embedding: vec![1.0, 0.0],
            confidence,
            provenance: Provenance::SingleObservation,
            layer,
            tested,
            domain: None,
            tier: "context".into(),
            support_count,
            distinct_sessions: 1,
            contradict_count: 0,
            pinned,
            last_confirmed_at: Some(created),
            consolidated_at: None,
            created_at: created,
            updated_at: created,
            session_id: None,
            embedding_model: crate::config::DEFAULT_EMBEDDING_MODEL.into(),
            kind: crate::store::beliefs::BeliefKind::Context,
        }
    }

    async fn is_retired(db: &Db, text: &str) -> bool {
        let h = crate::belief::synthesis::text_hash(text);
        db.is_text_suppressed(&h, Utc::now()).await.unwrap()
    }

    #[tokio::test]
    async fn retires_old_dead_beliefs_but_spares_live_ones() {
        let db = Db::open_in_memory().await.unwrap();
        // tested + decayed + old  -> retired
        let decayed = mk("decayed", true, 0.30, 61, 2, false, Layer::Context);
        // untested + single-observation + old -> retired
        let one_off = mk("oneoff", false, 0.70, 61, 1, false, Layer::Context);
        // untested but corroborated (support 2) -> kept
        let corroborated = mk("corrob", false, 0.70, 61, 2, false, Layer::Context);
        // old + dead but PINNED -> kept
        let pinned = mk("pinned", true, 0.10, 61, 1, true, Layer::Context);
        // old + dead but IDENTITY -> kept
        let identity = mk("ident", true, 0.10, 61, 1, false, Layer::Identity);
        // young (dead on paper, but not old enough) -> kept
        let young = mk("young", true, 0.10, 10, 1, false, Layer::Context);
        for b in [&decayed, &one_off, &corroborated, &pinned, &identity, &young] {
            db.insert_belief(b).await.unwrap();
        }

        run_maintenance(&db).await.unwrap();

        assert!(is_retired(&db, &decayed.text).await, "old decayed tested belief is retired");
        assert!(is_retired(&db, &one_off.text).await, "old uncorroborated untested belief is retired");
        assert!(!is_retired(&db, &corroborated.text).await, "corroborated untested belief is kept");
        assert!(!is_retired(&db, &pinned.text).await, "pinned belief is never retired");
        assert!(!is_retired(&db, &identity.text).await, "identity belief is never retired");
        assert!(!is_retired(&db, &young.text).await, "a belief younger than the horizon is kept");
    }

    #[tokio::test]
    async fn retirement_is_idempotent_across_passes() {
        let db = Db::open_in_memory().await.unwrap();
        let dead = mk("dead", false, 0.70, 61, 1, false, Layer::Context);
        db.insert_belief(&dead).await.unwrap();

        run_maintenance(&db).await.unwrap();
        assert!(is_retired(&db, &dead.text).await);
        let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM suppressions")
            .fetch_one(db.read_pool())
            .await
            .unwrap();

        // Force the nightly gate open again and re-run: must not stack a
        // second suppression row for the already-retired belief.
        db.set_setting(LAST_MAINTENANCE_KEY, "0").await.unwrap();
        run_maintenance(&db).await.unwrap();
        let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM suppressions")
            .fetch_one(db.read_pool())
            .await
            .unwrap();
        assert_eq!(before, after, "an already-suppressed belief is not re-suppressed");
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
