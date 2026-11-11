//! Background task: a single 60s interval running the idle sweep
//! (extract_and_record(IdleClose) on the unlearned tail, then consolidate)
//! and periodic maintenance. Spawned alongside the daemon scheduler, aborted
//! before agent shutdown.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::SqlitePool;
use tokio::sync::watch;

use crate::engine::PathwayEngine;
use crate::error::Result;
use crate::learn::{self, LearnRequest, LearnTrigger};
use crate::traits::{AlwaysLearn, LearnSchedule, SessionChat};

/// Seconds between ticks.
pub const TICK_INTERVAL_SECS: u64 = 60;

/// Max sessions touched per idle-sweep tick (a daemon restarting after a long
/// absence shouldn't fire dozens of constrained-decode requests at once).
/// Only sessions with work to do count against it.
pub const IDLE_SWEEP_BATCH: usize = 3;

/// How long a session whose learn pass failed sits out of the sweep. Without
/// it, a session the model keeps failing on would take a batch slot every
/// tick and starve the rest, the way the first three rows once did.
pub const LEARN_FAILURE_BACKOFF: Duration = Duration::from_secs(30 * 60);

/// Per-session failure times, kept by `run` across ticks.
pub type SweepBackoff = HashMap<String, Instant>;

/// Max beliefs re-embedded per tick (see `reembed_stale_beliefs`) -- each is
/// a real `/api/embeddings` round-trip to Ollama, so this is bounded the
/// same way `IDLE_SWEEP_BATCH` bounds extraction calls.
pub const EMBEDDING_MIGRATION_BATCH: i64 = 25;

/// Run the background loop until `shutdown_rx` fires. Errors are logged, not
/// fatal -- each tick is best-effort.
///
/// `app_id` is the app this graph belongs to (see `idle_session_ids`), and
/// `chat` picks each session's own model.
pub async fn run<C: SessionChat>(
    engine: Arc<PathwayEngine>,
    host_pool: SqlitePool,
    chat: C,
    app_id: Option<String>,
    schedule: Arc<dyn LearnSchedule>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(TICK_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut backoff = SweepBackoff::new();
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(e) = idle_sweep_with(&engine, &host_pool, &chat, app_id.as_deref(), &mut backoff, schedule.as_ref()).await {
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

/// One sweep: learn idle/stale-active sessions' unlearned tails and
/// consolidate them.
///
/// Only sessions with something to do take one of the `IDLE_SWEEP_BATCH`
/// slots: an unlearned tail, or conversation-layer beliefs still waiting to
/// be consolidated. Taking the first few ids regardless meant re-consolidating
/// the same three finished sessions every minute while every newer session
/// waited forever.
pub async fn idle_sweep<C: SessionChat>(
    engine: &PathwayEngine,
    host_pool: &SqlitePool,
    chat: &C,
    app_id: Option<&str>,
    backoff: &mut SweepBackoff,
) -> Result<()> {
    idle_sweep_with(engine, host_pool, chat, app_id, backoff, &AlwaysLearn).await
}

/// [`idle_sweep`] under a [`LearnSchedule`]: learn passes run only when the
/// schedule allows, up to its batch; consolidation runs regardless.
pub async fn idle_sweep_with<C: SessionChat>(
    engine: &PathwayEngine,
    host_pool: &SqlitePool,
    chat: &C,
    app_id: Option<&str>,
    backoff: &mut SweepBackoff,
    schedule: &dyn LearnSchedule,
) -> Result<()> {
    let learn_batch = schedule.learn_batch().await;
    // Query 2 (host seam): idle + stale-active session ids. Cutoffs are
    // computed in SQL against `bigtiny.db`'s own clock/format -- see the
    // doc comment on `idle_session_ids` for why a Rust-side `chrono` cutoff
    // bound as a query parameter doesn't compare correctly against it.
    const IDLE_MINUTES: i64 = 15;
    const ACTIVE_MINUTES: i64 = 30;
    // Each side is asked one set-based question per tick (sessions with
    // their newest rowid; every watermark; which sessions hold conversation
    // beliefs); only a session with something to do gets a look of its own.
    // Asked per session, an app with 1,000 chats cost thousands of queries a
    // minute, forever.
    let ids = crate::learn::host::idle_sessions_with_max_rowid(
        host_pool,
        app_id,
        IDLE_MINUTES,
        ACTIVE_MINUTES,
    )
    .await?;
    let watermarks = engine.db.all_learned_rowids().await.unwrap_or_default();
    let with_beliefs = engine
        .db
        .sessions_with_conversation_beliefs()
        .await
        .unwrap_or_default();
    backoff.retain(|_, failed_at| failed_at.elapsed() < LEARN_FAILURE_BACKOFF);
    let mut taken = 0usize;
    let batch = learn_batch.unwrap_or(IDLE_SWEEP_BATCH).max(1);
    for (session_id, max_rowid) in &ids {
        if taken >= batch {
            break;
        }
        if backoff.contains_key(session_id) {
            continue;
        }
        let watermark = watermarks.get(session_id).copied().unwrap_or(0);
        let max_rowid = *max_rowid;
        let needs_learning = learn_batch.is_some() && max_rowid > watermark;
        let needs_consolidation = with_beliefs.contains(session_id);
        if !needs_learning && !needs_consolidation {
            continue;
        }
        // Skip paused sessions.
        if engine.is_paused(session_id).await.unwrap_or(false) {
            continue;
        }
        taken += 1;
        // Failures are logged, not propagated: one bad session must not stop
        // the sweep. They used to be discarded outright (`let _ =`), which is
        // how pathway went weeks committing nothing without a single log line.
        if needs_learning {
            schedule.working();
            let session_chat = chat.for_session(session_id).await;
            if let Err(e) = learn::extract_and_record(
                engine,
                host_pool,
                &session_chat,
                LearnRequest {
                    session_id,
                    through_rowid: max_rowid,
                    given_chunk: None,
                },
                LearnTrigger::IdleClose,
            )
            .await
            {
                tracing::warn!("pathway idle learn failed for session {session_id}: {e}");
                backoff.insert(session_id.clone(), Instant::now());
            }
        }
        if let Err(e) = crate::consolidate::consolidate_session(&engine.db, session_id).await {
            tracing::warn!("pathway consolidate failed for session {session_id}: {e}");
        }
    }
    // Periodic maintenance (cheap; runs each sweep).
    let _ = crate::maintenance::run_maintenance_with(&engine.db, engine.cfg.recall.ewma_alpha).await;
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
