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

/// Max beliefs re-embedded per tick (see `reembed_stale_beliefs`) -- each is
/// a real `/api/embeddings` round-trip to Ollama, so this is bounded the
/// same way `IDLE_SWEEP_BATCH` bounds extraction calls.
pub const EMBEDDING_MIGRATION_BATCH: i64 = 25;

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
        // The session is being closed out -- drop its trajectory-embedding
        // state (see `PathwayEngine::forget_trajectory`) so that map stays
        // bounded by concurrently-active sessions, not every session ever
        // seen.
        engine.forget_trajectory(session_id);
    }
    // Periodic maintenance (cheap; runs each sweep).
    let _ = crate::maintenance::run_maintenance(&engine.db).await;
    // Bound the per-session learn-lock map's growth (see
    // `PathwayEngine::prune_idle_learn_locks`) -- cheap, safe every tick.
    engine.prune_idle_learn_locks();
    // Batch re-embed any beliefs still tagged with a stale embedding model
    // (see `migrations/005_belief_embedding_model.sql`). Best-effort and
    // logged-not-fatal like everything else in this sweep.
    if let Err(e) = reembed_stale_beliefs(engine).await {
        tracing::warn!("embedding-model re-embed pass failed: {e}");
    }
    Ok(())
}

/// Re-embed up to `EMBEDDING_MIGRATION_BATCH` beliefs whose `embedding_model`
/// doesn't match the currently configured model (a belief row's own column
/// value *is* the re-embedding queue -- see
/// `store::beliefs::list_stale_embedding_beliefs`). Skips the whole pass if
/// Ollama isn't reachable this tick, rather than letting individual
/// `embed()` calls silently fall back to the lexical hashing embedder and
/// get tagged as `current_model` anyway -- that would mix two incompatible
/// embedding spaces under one label, exactly the class of bug this
/// migration exists to prevent. Deliberately does not touch `updated_at`/
/// `last_confirmed_at` (see `Db::update_embedding`'s doc comment).
pub async fn reembed_stale_beliefs(engine: &PathwayEngine) -> Result<()> {
    let current_model = engine.cfg.embedding.ollama_model.clone();
    let stale = engine.db.list_stale_embedding_beliefs(&current_model, EMBEDDING_MIGRATION_BATCH).await?;
    if stale.is_empty() {
        return Ok(());
    }
    if !engine.embed.probe_semantic().await {
        return Ok(());
    }
    for belief in &stale {
        // If the embedder falls back to the lexical hash vectorizer mid-batch
        // (a per-item timeout right after the probe succeeded), do NOT persist
        // that hash-space vector tagged as `current_model` — overwriting a
        // real semantic embedding with a garbage-space vector permanently is
        // worse than leaving the stale-tagged row to be retried. The
        // batch-level `probe_semantic()` above only guarantees Ollama is up, not
        // that every call stays semantic.
        // Bypass the cache: `probe_semantic()` just confirmed Ollama is up, but
        // if this belief's text is still cache-resident from before the
        // outage, a cache-checking call would keep returning that stale
        // hash-fallback vector forever instead of actually retrying now.
        let (embedding, semantic) = engine.embed.embed_fresh_with_space(&belief.text).await;
        if !semantic {
            tracing::warn!(
                belief_id = %belief.id,
                "skipped re-embed: embedder fell back to lexical space"
            );
            continue;
        }
        if let Err(e) = engine.db.update_embedding(&belief.id, &embedding, &current_model).await {
            tracing::warn!(belief_id = %belief.id, "re-embedding failed to persist: {e}");
        }
    }
    Ok(())
}
