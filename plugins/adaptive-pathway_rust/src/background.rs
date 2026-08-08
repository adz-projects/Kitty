//! Background task: a single 60s interval running the idle sweep
//! (extract_and_record(IdleClose) on the unlearned tail, then consolidate)
//! and periodic maintenance. Spawned alongside the daemon scheduler, aborted
//! before agent shutdown.

use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::watch;

use crate::engine::PathwayEngine;
use crate::error::Result;
use crate::learn::{self, LearnRequest, LearnTrigger};
use crate::traits::StructuredChat;

/// Seconds between ticks.
pub const TICK_INTERVAL_SECS: u64 = 60;

/// Max sessions touched per idle-sweep tick (a daemon restarting after a long
/// absence shouldn't fire dozens of constrained-decode requests at once).
pub const IDLE_SWEEP_BATCH: usize = 3;

/// Run the background loop until `shutdown_rx` fires. Errors are logged, not
/// fatal -- each tick is best-effort.
pub async fn run<S: StructuredChat>(
    engine: Arc<PathwayEngine>,
    host_pool: SqlitePool,
    chat: Arc<S>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(TICK_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(e) = idle_sweep(&engine, &host_pool, chat.as_ref()).await {
                    tracing::warn!("pathway background sweep failed: {e}");
                }
            }
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    tracing::info!("pathway background task shutting down");
                    break;
                }
            }
        }
    }
}

/// One sweep: consolidate idle/stale-active sessions' unlearned tails.
pub async fn idle_sweep<S: StructuredChat>(
    engine: &PathwayEngine,
    host_pool: &SqlitePool,
    chat: &S,
) -> Result<()> {
    // Query 2 (host seam): idle + stale-active session ids. Cutoffs are
    // computed in SQL against `bigtiny.db`'s own clock/format -- see the
    // doc comment on `idle_session_ids` for why a Rust-side `chrono` cutoff
    // bound as a query parameter doesn't compare correctly against it.
    const IDLE_MINUTES: i64 = 15;
    const ACTIVE_MINUTES: i64 = 30;
    let ids = crate::learn::host::idle_session_ids(host_pool, IDLE_MINUTES, ACTIVE_MINUTES).await?;
    for session_id in ids.iter().take(IDLE_SWEEP_BATCH) {
        // Skip paused sessions.
        if engine.is_paused(session_id).await.unwrap_or(false) {
            continue;
        }
        let watermark = engine.db.last_learned_rowid(session_id).await.unwrap_or(0);
        let max_rowid = crate::learn::host::session_max_rowid(host_pool, session_id).await.unwrap_or(watermark);
        if max_rowid > watermark {
            let _ = learn::extract_and_record(
                engine,
                host_pool,
                chat,
                LearnRequest {
                    session_id,
                    through_rowid: max_rowid,
                    given_chunk: None,
                },
                LearnTrigger::IdleClose,
            )
            .await;
        }
        let _ = crate::consolidate::consolidate_session(&engine.db, session_id).await;
    }
    // Periodic maintenance (cheap; runs each sweep).
    let _ = crate::maintenance::run_maintenance(&engine.db).await;
    // Bound the per-session learn-lock map's growth (see
    // `PathwayEngine::prune_idle_learn_locks`) -- cheap, safe every tick.
    engine.prune_idle_learn_locks();
    Ok(())
}
