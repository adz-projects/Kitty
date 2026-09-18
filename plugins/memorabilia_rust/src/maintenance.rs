//! Maintenance scheduler pass (project-plan.md §11, §15 Phase 8): the work
//! the plugin's background sweep drives on a cadence. One call is
//! [`Engine::maintenance_tick`]; the host owns the cadence and passes `now`
//! (never the wall clock) so state stays deterministic under tests.
//!
//! Per tick (cheap, every cadence): drain the reinforcement outbox, resolve
//! decayed chunks into `archived` (closing their DISPUTED edges and
//! transitioning any now-unsupported propositions), and hard-delete
//! archived rows past `archive_retention_days`. Behind a persisted 24h gate
//! (`last_maintenance_at`): the heavy pass — `reembed_stale` and transient →
//! static promotion.
//!
//! Soft-fail by construction (claude.md principle 9): each phase logs and
//! swallows its own errors and returns a count, so one failing phase never
//! aborts the rest of the tick, and a tick never surfaces an error to the
//! caller.
//!
//! Not yet implemented (documented follow-ups, plan §4.3 / §9.3): reliability
//! drift needs a dispute-outcome ledger that does not exist yet, and the
//! `RHYMES_WITH` pool is Phase 9. Both are intentionally out of this pass.

use chrono::NaiveDateTime;

use crate::core;
use crate::engine::Engine;
use crate::error::Result;

/// Seconds in an hour / a day — named per the no-bare-literals rule.
const SECONDS_PER_HOUR: i64 = 3_600;
const SECONDS_PER_DAY: i64 = 86_400;

/// Observable result of one maintenance tick (for logging and tests).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaintenanceOutcome {
    pub outbox_grounded: usize,
    pub outbox_reinforced: usize,
    pub chunks_archived: usize,
    pub propositions_archived: usize,
    pub propositions_deleted: usize,
    pub disputes_closed: usize,
    pub chunks_hard_deleted: usize,
    pub propositions_hard_deleted: usize,
    pub heavy_ran: bool,
    pub reembedded: usize,
    pub promoted: usize,
}

/// Max outbox rows drained per tick — the drain is bounded like the
/// extraction batch so a tick never holds the sweep open across a whole
/// backlog (plan §11).
const OUTBOX_DRAIN_LIMIT: i64 = 256;

impl Engine {
    /// One maintenance pass at caller time `now` (ISO-8601 UTC, second
    /// precision). Never returns an error — an unparseable `now` or any phase
    /// failure is logged and the tick yields whatever it completed.
    pub async fn maintenance_tick(&self, now: &str) -> MaintenanceOutcome {
        let mut outcome = MaintenanceOutcome::default();
        let Some(now_naive) = core::parse_utc(now) else {
            tracing::warn!("maintenance_tick: unparseable now ({now}); tick skipped");
            return outcome;
        };

        match self.drain_outbox(now).await {
            Ok((g, r)) => {
                outcome.outbox_grounded = g;
                outcome.outbox_reinforced = r;
            }
            Err(e) => tracing::warn!("maintenance: outbox drain failed ({e})"),
        }

        match self.resolve_decay(now, &now_naive).await {
            Ok((archived, p_arch, p_del, disputes)) => {
                outcome.chunks_archived = archived;
                outcome.propositions_archived = p_arch;
                outcome.propositions_deleted = p_del;
                outcome.disputes_closed = disputes;
            }
            Err(e) => tracing::warn!("maintenance: decay resolution failed ({e})"),
        }

        match self.hard_delete_expired(&now_naive).await {
            Ok((c, p)) => {
                outcome.chunks_hard_deleted = c;
                outcome.propositions_hard_deleted = p;
            }
            Err(e) => tracing::warn!("maintenance: hard-delete pass failed ({e})"),
        }

        if self.heavy_pass_due(&now_naive).await {
            outcome.heavy_ran = true;
            match self.reembed_stale().await {
                Ok(n) => outcome.reembedded = n,
                Err(e) => tracing::warn!("maintenance: reembed_stale failed ({e})"),
            }
            match self.promote_transient(now).await {
                Ok(n) => outcome.promoted = n,
                Err(e) => tracing::warn!("maintenance: promotion failed ({e})"),
            }
            if let Err(e) = self.db.set_setting("last_maintenance_at", now).await {
                tracing::warn!("maintenance: could not persist heavy-pass anchor ({e})");
            }
        }

        outcome
    }

    /// Drain the reinforcement outbox (plan §6.2): apply grounding and
    /// reinforcement counters (reinforcement capped at
    /// `reinforcement_count_max`; a reinforced non-deadline chunk resets its
    /// decay anchor to `now` — plan §7.1), then delete the delivered rows.
    /// At-least-once + `UNIQUE(chunk_id, query_event_id)` makes replay
    /// idempotent; applying here is the "exactly-once effect" step.
    async fn drain_outbox(&self, now: &str) -> Result<(usize, usize)> {
        let batch = self.db.next_outbox_batch(OUTBOX_DRAIN_LIMIT).await?;
        if batch.is_empty() {
            return Ok((0, 0));
        }
        let max = self.config.reinforcement_count_max as i64;
        let mut grounded = 0usize;
        let mut reinforced = 0usize;
        // Group deletions by query_event_id.
        let mut by_event: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for row in &batch {
            by_event
                .entry(row.query_event_id.clone())
                .or_default()
                .push(row.chunk_id.clone());
            let Some(chunk) = self.db.get_chunk(&row.chunk_id).await? else {
                continue; // chunk hard-deleted meanwhile; the row still clears
            };
            let g = if row.grounded { 1 } else { 0 };
            let do_reinforce = row.reinforced && chunk.reinforcement_count < max;
            let r = if do_reinforce { 1 } else { 0 };
            // Reinforcement resets the anchor (not τ), but never for a
            // deadline chunk whose decay is anchored on T_expire.
            let anchor = (do_reinforce && chunk.decay_class != "deadline").then_some(now);
            self.db
                .apply_chunk_counters(&row.chunk_id, g, r, anchor)
                .await?;
            grounded += g as usize;
            reinforced += r as usize;
        }
        for (event, chunk_ids) in by_event {
            self.db.delete_outbox_pairs(&event, &chunk_ids).await?;
        }
        Ok((grounded, reinforced))
    }

    /// Archive chunks that have decayed below the prune threshold (or whose
    /// deadline window has elapsed), cascading: close each archived chunk's
    /// open DISPUTED edges (§9.1 — archived materials drop out of dispute),
    /// then transition every proposition that just lost its last active
    /// supporter (§5.3/§7.4: `high`/`unknown` → `UNSUPPORTED_ARCHIVE`, else
    /// delete) and recompute the confidence of those still supported.
    async fn resolve_decay(
        &self,
        now: &str,
        now_naive: &NaiveDateTime,
    ) -> Result<(usize, usize, usize, usize)> {
        let cfg = &self.config;
        let mut archived = 0usize;
        let mut props_archived = 0usize;
        let mut props_deleted = 0usize;
        let mut disputes_closed = 0usize;

        for chunk in self.db.list_chunks_by_status("active").await? {
            let anchor = match core::parse_utc(&chunk.anchor_at) {
                Some(a) => a,
                None => continue,
            };
            let expiry = chunk.urgency_expires_at.as_deref().and_then(core::parse_utc);
            let decay = core::decay(&chunk.decay_class, &anchor, expiry, *now_naive, cfg).unwrap_or(0.0);
            if !core::should_archive_chunk(
                &chunk.decay_class,
                decay,
                cfg.activation_prune_threshold,
                expiry,
                *now_naive,
                cfg,
            ) {
                continue;
            }

            // Nodes this chunk supported (before archiving; support links
            // survive archive, so capture the set now).
            let supported = self.db.list_nodes_supported_by_chunk(&chunk.chunk_id).await?;
            // Other endpoints of its open disputes, to refresh once lifted.
            let open = self.db.list_open_disputes_for_chunk(&chunk.chunk_id).await?;

            self.db.archive_chunk(&chunk.chunk_id, now).await?;
            let _ = self.vectors.remove(&chunk.chunk_id).await; // best-effort

            for edge in &open {
                self.db.close_disputed_edge(&edge.edge_id, now).await?;
                disputes_closed += 1;
                let other = if edge.chunk_a == chunk.chunk_id {
                    &edge.chunk_b
                } else {
                    &edge.chunk_a
                };
                for node in self.db.list_nodes_supported_by_chunk(other).await? {
                    self.recompute_node_confidence(&node, now_naive).await?;
                }
            }

            for node in supported {
                if self.db.list_active_supporting_chunks(&node).await?.is_empty() {
                    // N_active = 0 → leave the active graph (plan §5.3).
                    let Some(prop) = self.db.get_proposition(&node).await? else {
                        continue;
                    };
                    match core::unsupported_transition(&prop.importance) {
                        "UNSUPPORTED_ARCHIVE" => {
                            self.db.archive_proposition(&node, now).await?;
                            props_archived += 1;
                        }
                        _ => {
                            self.db.delete_proposition(&node).await?;
                            props_deleted += 1;
                        }
                    }
                } else {
                    self.recompute_node_confidence(&node, now_naive).await?;
                }
            }
            archived += 1;
        }
        Ok((archived, props_archived, props_deleted, disputes_closed))
    }

    /// Hard-delete archived chunks and unsupported-archive propositions whose
    /// `archived_at` is older than `archive_retention_days` (plan §7.4). Chunk
    /// FK cascades remove its support links, DISPUTED edges, and outbox rows;
    /// its vector row is dropped through the seam first (the vtab has no FKs).
    async fn hard_delete_expired(&self, now_naive: &NaiveDateTime) -> Result<(usize, usize)> {
        let cutoff_secs = self.config.archive_retention_days as i64 * SECONDS_PER_DAY;
        let mut chunks = 0usize;
        let mut props = 0usize;

        for chunk in self.db.list_chunks_by_status("archived").await? {
            let Some(at) = chunk.archived_at.as_deref().and_then(core::parse_utc) else {
                continue;
            };
            if (*now_naive - at).num_seconds() >= cutoff_secs {
                let _ = self.vectors.remove(&chunk.chunk_id).await;
                self.db.delete_chunk(&chunk.chunk_id).await?;
                chunks += 1;
            }
        }
        for prop in self.db.list_propositions_by_status("UNSUPPORTED_ARCHIVE").await? {
            let Some(at) = prop.archived_at.as_deref().and_then(core::parse_utc) else {
                continue;
            };
            if (*now_naive - at).num_seconds() >= cutoff_secs {
                self.db.delete_proposition(&prop.node_id).await?;
                props += 1;
            }
        }
        Ok((chunks, props))
    }

    /// True when the persisted 24h heavy-pass gate has elapsed (plan §11).
    /// A never-run engine runs the heavy pass on its first tick.
    async fn heavy_pass_due(&self, now_naive: &NaiveDateTime) -> bool {
        let interval = self.config.maintenance_heavy_interval_hours as i64 * SECONDS_PER_HOUR;
        match self.db.get_setting("last_maintenance_at").await {
            Ok(Some(last)) => match core::parse_utc(&last) {
                Some(t) => (*now_naive - t).num_seconds() >= interval,
                None => true,
            },
            Ok(None) => true,
            Err(_) => false,
        }
    }

    /// Re-embed chunks whose vectors are still lexical-fallback rows once the
    /// semantic embedder has recovered (plan §3.3 `reembed_stale`). A vector
    /// that still comes back tagged `__lexical_hash__` is left as is.
    async fn reembed_stale(&self) -> Result<usize> {
        let stale = self
            .vectors
            .list_by_model(crate::config::HASH_EMBED_MODEL)
            .await
            .map_err(crate::error::Error::Internal)?;
        let mut count = 0usize;
        for chunk_id in stale {
            let Some(chunk) = self.db.get_chunk(&chunk_id).await? else {
                continue;
            };
            if chunk.status != "active" {
                continue;
            }
            let (vec, tag) = match self.embedder.embed_fresh(&chunk.content).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("reembed_stale: embed failed for {chunk_id} ({e})");
                    continue;
                }
            };
            if tag == crate::config::HASH_EMBED_MODEL {
                continue; // still down; keep the fallback vector
            }
            self.vectors
                .upsert(&chunk_id, &vec, &tag)
                .await
                .map_err(crate::error::Error::Internal)?;
            self.db.update_chunk_embedding_model(&chunk_id, &tag).await?;
            count += 1;
        }
        Ok(count)
    }

    /// Promote transient chunks to static when all four §6.1 gates hold:
    /// `reinforcement_count ≥` threshold, no open dispute,
    /// `source_reliability ≥` floor, and ≥ 2 distinct `source_entity`
    /// corroborating one of its propositions. Frequency alone never promotes.
    async fn promote_transient(&self, _now: &str) -> Result<usize> {
        let cfg = &self.config;
        let mut promoted = 0usize;
        for chunk in self.db.list_chunks_by_status("active").await? {
            if chunk.decay_class != "transient" {
                continue;
            }
            let open_dispute = self
                .db
                .max_open_dispute_strength(&chunk.chunk_id)
                .await?
                .is_some();
            // Corroboration: the best distinct-source count over the
            // propositions this chunk supports.
            let mut distinct_sources = 0i64;
            for node in self.db.list_nodes_supported_by_chunk(&chunk.chunk_id).await? {
                distinct_sources = distinct_sources.max(self.db.count_active_sources(&node).await?);
            }
            if core::can_promote(
                chunk.reinforcement_count,
                open_dispute,
                chunk.source_reliability,
                distinct_sources,
                false,
                cfg,
            ) {
                self.db.set_chunk_decay_class(&chunk.chunk_id, "static").await?;
                promoted += 1;
            }
        }
        Ok(promoted)
    }
}
