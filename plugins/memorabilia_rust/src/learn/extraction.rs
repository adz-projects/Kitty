//! Stage 4 asynchronous extraction (plan §15 Phase 6, §3.5/§3.6): the
//! pending-queue drain.
//!
//! Per chunk: Local Neighborhood Probe (bounded contradiction candidates) →
//! prompt with `POTENTIAL_CONFLICTS` → extraction model through the
//! `StructuredChat` seam behind the global semaphore and the `timeout_s`
//! budget → validated JSON → one transaction writing DISPUTED edges,
//! propositions (confidence from `core`), support links, and the chunk's
//! decay profile; then a confidence refresh for every proposition a new
//! edge touches (an opened edge changes both endpoints' weights).
//!
//! Soft-fail by construction (claude.md principle 9): a chat error,
//! timeout, malformed entry, or DB error is logged and contained to the
//! chunk — it stays `pending` with its retry backoff started, and the rest
//! of the batch proceeds. `ingest()` never calls this path (Phase 4 exit
//! criterion); the maintenance tick (Phase 8) and tests drive
//! [`Engine::drain_extraction`].

use std::collections::{BTreeMap, HashSet};

use chrono::{Duration as ChronoDuration, NaiveDateTime};
use serde_json::{json, Value};

use crate::core;
use crate::engine::Engine;
use crate::error::{Error, Result};
use crate::store::chunks::Chunk;
use crate::store::edges::DisputedEdge;
use crate::store::propositions::Proposition;
use crate::text::{normalize_text, sha256_hex};

/// Upper cosine bound of the conflict-probe window (plan §3.5:
/// `[conflict_probe_similarity_min, 0.98]`). Above it the pair is a
/// near-duplicate — clustering/dedup territory, not dispute territory.
/// Fixed by the spec, named per the no-bare-literals rule.
const CONFLICT_PROBE_SIMILARITY_MAX: f64 = 0.98;

/// Store timestamp format (migration 002 convention: ISO-8601 UTC, second
/// precision, literal `Z`).
const STORE_TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";

/// One probe neighbor as rendered into the prompt (plan §3.5: the chunk's
/// text AND its extracted propositions).
#[derive(Debug, Clone)]
pub struct NeighborView {
    pub chunk_id: String,
    pub source_entity: String,
    pub content: String,
    pub claims: Vec<String>,
}

/// Outcome of one [`Engine::drain_extraction`] call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrainOutcome {
    /// Chunks taken from the ready queue this pass.
    pub attempted: usize,
    /// Chunks that reached a terminal write (`done`, suppressed-skip
    /// included).
    pub succeeded: usize,
    /// Chunks that failed this pass (stayed `pending`, backoff started).
    pub failed: usize,
    /// Suppressed chunks drained with no LLM call and no writes.
    pub skipped_suppressed: usize,
    /// Proposition rows actually written (dedup no-ops excluded).
    pub propositions_written: usize,
    /// DISPUTED edge rows actually opened (re-opens excluded).
    pub disputes_opened: usize,
}

/// The extraction output contract. Schema discipline mirrors the
/// reference's `extraction_schema`: every field `required`, empty-string
/// sentinels instead of nullable unions — a total grammar is what small
/// local models decode reliably.
pub fn extraction_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "propositions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "claim": {"type": "string", "description": "One atomic, self-contained factual claim stated or directly implied by the chunk. Third person, present tense. Never PII, never meta-commentary."},
                        "importance": {"type": "string", "enum": ["high", "medium", "low", "unknown"]},
                        "urgency": {"type": "string", "enum": ["high", "medium", "low", "unknown"]},
                        "decay_class": {"type": "string", "enum": ["static", "transient", "deadline"]},
                        "urgency_expires_at": {"type": "string", "description": "UTC ISO-8601 second precision (2026-08-20T23:59:59Z) when decay_class is deadline, else empty string."}
                    },
                    "required": ["claim", "importance", "urgency", "decay_class", "urgency_expires_at"]
                }
            },
            "disputes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "chunk_id": {"type": "string", "description": "The chunk_id of a POTENTIAL_CONFLICTS entry whose claim DIRECTLY contradicts one of your extracted claims. No other chunk_id is valid."},
                        "strength": {"type": "number", "description": "Confidence in [0, 1] that a direct contradiction exists."},
                        "reason": {"type": "string", "description": "One line: which claims contradict."}
                    },
                    "required": ["chunk_id", "strength", "reason"]
                }
            }
        },
        "required": ["propositions", "disputes"]
    })
}

/// The two-message extraction prompt (plan §3.6 / §3.5).
pub fn build_extraction_prompt(chunk: &Chunk, neighbors: &[NeighborView]) -> Vec<Value> {
    let system = "You are the extraction model for a factual memory system. \
        Read the chunk and extract its atomic, self-contained factual claims. \
        For each claim infer: importance (high/medium/low/unknown), urgency \
        (high/medium/low/unknown), and a decay class — \"static\" for a \
        long-lived fact, \"transient\" for recent context that will go stale, \
        \"deadline\" for something tied to a specific future expiry (then set \
        urgency_expires_at as UTC ISO-8601 second precision; empty string \
        otherwise). If a POTENTIAL_CONFLICTS entry directly contradicts one \
        of your claims, add a disputes entry naming that entry's chunk_id, \
        with your confidence in [0, 1] and a one-line reason. Never invent \
        claims the chunk does not support. Return JSON only, matching the \
        schema.";
    let mut user = format!(
        "CHUNK (source_entity: {}):\n{}\n\nPOTENTIAL_CONFLICTS:\n",
        chunk.source_entity, chunk.content
    );
    if neighbors.is_empty() {
        user.push_str("(none)\n");
    } else {
        for (i, n) in neighbors.iter().enumerate() {
            user.push_str(&format!(
                "[{}] chunk_id: {} (source_entity: {})\ntext: {}\nknown propositions:\n",
                i + 1,
                n.chunk_id,
                n.source_entity,
                n.content
            ));
            if n.claims.is_empty() {
                user.push_str("(none yet)\n");
            } else {
                for claim in &n.claims {
                    user.push_str(&format!("- {claim}\n"));
                }
            }
        }
    }
    vec![
        json!({"role": "system", "content": system}),
        json!({"role": "user", "content": user}),
    ]
}

/// A validated, schema-conformant claim ready to write.
struct ValidClaim {
    node_id: String,
    claim: String,
    importance: String,
    urgency: String,
    decay_class: String,
    urgency_expires_at: Option<NaiveDateTime>,
}

/// Per-entry claim outcome (soft-fail counting: a bad entry is dropped,
/// never fatal — a `deadline` claim without a valid `T_expire` would
/// violate the CHECK-enforced pairing on write).
enum ClaimVerdict {
    Kept(ValidClaim),
    Tombstoned,
    Dropped,
}

fn level_or(raw: &Value, field: &str) -> String {
    match raw.get(field).and_then(|v| v.as_str()) {
        Some("high") => "high".into(),
        Some("medium") => "medium".into(),
        Some("low") => "low".into(),
        _ => "unknown".into(),
    }
}

fn verify_claim(raw: &Value, tombstones: &HashSet<String>) -> ClaimVerdict {
    let claim = raw.get("claim").and_then(|v| v.as_str()).unwrap_or("");
    let norm = normalize_text(claim);
    if norm.is_empty() {
        return ClaimVerdict::Dropped;
    }
    // plan §12.2: tombstone check against the normalized proposition text —
    // forgotten text is never relearned, extraction included.
    if tombstones.contains(&sha256_hex(&norm)) {
        return ClaimVerdict::Tombstoned;
    }
    let importance = level_or(raw, "importance");
    let urgency = level_or(raw, "urgency");
    let decay_class = match raw.get("decay_class").and_then(|v| v.as_str()) {
        Some("static") => "static",
        Some("deadline") => "deadline",
        _ => "transient",
    };
    let expiry_raw = raw
        .get("urgency_expires_at")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let expiry = match core::parse_utc(expiry_raw) {
        Some(dt) if decay_class == "deadline" => Some(dt),
        Some(_) => None, // a non-deadline claim carries no T_expire
        None => {
            if decay_class == "deadline" {
                return ClaimVerdict::Dropped; // would violate the CHECK pairing
            }
            None
        }
    };
    // **Consolidation step.** The node_id is content-addressed by the
    // normalized claim ALONE — not by `(chunk_id, claim)` — so the same claim
    // extracted from a different chunk or a different source resolves to the
    // same proposition. Its `insert_proposition` then no-ops (ON CONFLICT DO
    // NOTHING) while `add_support_link` attaches the new chunk, and the
    // transaction's tail `recompute_node_confidence` (chunk_id is always a
    // recompute endpoint) folds every active supporter into one confidence
    // grouped by distinct source. That is what turns the §8.2 multi-source
    // noisy-OR and the §6.1 "≥ 2 distinct sources" promotion gate from latent
    // into live: two sources asserting the same fact now corroborate one
    // assertion instead of creating two single-source ones. A chunk's own
    // re-extraction retry still dedups (same claim → same id). Metadata
    // (importance/urgency/expiry) stays the first writer's — a deliberate
    // simplification; the earliest-deadline/importance-merge refinement is a
    // separate follow-up.
    let node_id = format!("p_{}", &sha256_hex(&norm)[..24]);
    ClaimVerdict::Kept(ValidClaim {
        node_id,
        claim: norm,
        importance,
        urgency,
        decay_class: decay_class.into(),
        urgency_expires_at: expiry,
    })
}

/// The chunk-level decay profile Stage 4 assigns (decay lives on chunks,
/// never propositions — claude.md principle 1). Deterministic resolution
/// across the chunk's claims: the earliest valid deadline wins (deadline
/// class is the strongest temporal reading), then a static reading, then
/// the transient default.
fn resolve_decay_profile(claims: &[ValidClaim]) -> (String, Option<NaiveDateTime>) {
    let expiries: Vec<NaiveDateTime> = claims
        .iter()
        .filter_map(|c| c.urgency_expires_at)
        .collect();
    if let Some(t) = core::earliest_deadline(&expiries) {
        return ("deadline".into(), Some(t));
    }
    if claims.iter().any(|c| c.decay_class == "static") {
        return ("static".into(), None);
    }
    ("transient".into(), None)
}

/// One chunk's terminal-extraction result (before drain-level accounting).
struct ExtractedCounts {
    propositions: usize,
    disputes: usize,
    suppressed_skip: bool,
}

impl Engine {
    /// Drain the ready `pending` queue (plan §3.6): oldest-first, bounded
    /// by `extraction.batch_size`, per-chunk retry backoff respected, one
    /// extraction completion at a time globally. `now` is the caller's UTC
    /// timestamp (ISO-8601 second precision) — never the wall clock — so
    /// test state stays deterministic and the scheduler owns time.
    pub async fn drain_extraction(&self, now: &str) -> DrainOutcome {
        let mut outcome = DrainOutcome::default();
        let now_naive = match core::parse_utc(now) {
            Some(dt) => dt,
            None => {
                tracing::warn!("drain_extraction: unparseable now ({now}); drain skipped");
                return outcome;
            }
        };
        let cfg = &self.config;

        // Per-chunk backoff bound: a failed chunk re-enters the ready
        // queue `retry_backoff_s` after its failure (ISO strings compare
        // lexicographically = chronologically).
        let ready_before =
            (now_naive - ChronoDuration::seconds(cfg.extraction.retry_backoff_s as i64))
                .format(STORE_TIMESTAMP_FORMAT)
                .to_string();

        let batch = match self
            .db
            .list_ready_pending_chunks(cfg.extraction.batch_size as i64, &ready_before)
            .await
        {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("drain_extraction: pending list failed ({e})");
                return outcome;
            }
        };

        // One tombstone-list load per pass keeps the per-claim gate off
        // the SQL path (tombstones change only on forget events).
        let tombstones = match self.db.list_tombstone_hashes().await {
            Ok(h) => HashSet::from_iter(h),
            Err(e) => {
                tracing::warn!("drain_extraction: tombstone list failed ({e}); gate disabled");
                HashSet::new()
            }
        };

        for chunk in batch {
            outcome.attempted += 1;
            match self.extract_chunk(&chunk, &tombstones, now, &now_naive).await {
                Ok(counts) => {
                    outcome.succeeded += 1;
                    outcome.propositions_written += counts.propositions;
                    outcome.disputes_opened += counts.disputes;
                    if counts.suppressed_skip {
                        outcome.skipped_suppressed += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "drain_extraction: chunk {} failed ({e}); stays pending, backoff started",
                        chunk.chunk_id
                    );
                    outcome.failed += 1;
                    if let Err(e) = self.db.mark_extraction_failed(&chunk.chunk_id, now).await {
                        tracing::warn!("drain_extraction: failure write failed ({e})");
                    }
                }
            }
        }
        outcome
    }

    /// One chunk's Stage 4 pass. `Ok` = reached a terminal state (done —
    /// including the suppressed no-op); `Err` = stays pending (the caller
    /// starts the retry backoff).
    async fn extract_chunk(
        &self,
        chunk: &Chunk,
        tombstones: &HashSet<String>,
        now: &str,
        now_naive: &NaiveDateTime,
    ) -> Result<ExtractedCounts> {
        // Re-read: the batch is a snapshot; the row may have been hard-
        // deleted (private forget) or archived meanwhile.
        let chunk = match self.db.get_chunk(&chunk.chunk_id).await? {
            Some(c) if c.status == "active" => c,
            _ => {
                // No-op write if the row is gone; stops a doomed re-select
                // if it remains.
                let _ = self.db.mark_extraction_done(&chunk.chunk_id).await;
                return Ok(ExtractedCounts {
                    propositions: 0,
                    disputes: 0,
                    suppressed_skip: false,
                });
            }
        };

        // Suppressed text is never extracted — not even sent to the
        // extraction model (claude.md principle 6: what the user marked
        // wrong/outdated is not re-distilled into propositions). Drain as
        // done-with-nothing so the queue stays healthy; re-ingestion after
        // an `outdated` suppression expires re-learns deliberately.
        let suppressed_ids = self.db.list_suppressed_chunk_ids(now).await?;
        if suppressed_ids.iter().any(|id| id == &chunk.chunk_id) {
            self.db.mark_extraction_done(&chunk.chunk_id).await?;
            self.audit_extracted(now, &chunk.chunk_id, 0, 0, true).await;
            return Ok(ExtractedCounts {
                propositions: 0,
                disputes: 0,
                suppressed_skip: true,
            });
        }

        // Local Neighborhood Probe (plan §3.5).
        let neighbors = self.probe_neighbors(&chunk).await?;

        // The LLM call: the global semaphore is scoped to exactly the
        // completion (never held across the DB writes), plus the
        // defensive timeout — the seam itself may have none internally
        // (the trait is also implemented by mocks with no timeout).
        let parsed = {
            let _permit = self
                .extraction_semaphore
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| Error::Internal("extraction semaphore closed".into()))?;
            let budget = std::time::Duration::from_secs(self.config.extraction.timeout_s as u64);
            tokio::time::timeout(budget, self.chat.structured_chat(
                build_extraction_prompt(&chunk, &neighbors),
                &extraction_schema(),
            ))
            .await
            .map_err(|_| {
                Error::Extract(format!(
                    "structured_chat timed out after {}s",
                    self.config.extraction.timeout_s
                ))
            })?
            .map_err(Error::Chat)?
        };

        // Validate claims (soft per-entry drops, counted).
        let raw_claims = parsed
            .get("propositions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut claims: Vec<ValidClaim> = Vec::new();
        let mut tombstoned = 0usize;
        let mut dropped = 0usize;
        for raw in &raw_claims {
            match verify_claim(raw, tombstones) {
                ClaimVerdict::Kept(c) => claims.push(c),
                ClaimVerdict::Tombstoned => tombstoned += 1,
                ClaimVerdict::Dropped => dropped += 1,
            }
        }
        if tombstoned > 0 {
            tracing::warn!(
                "extraction: {} tombstoned claim(s) skipped for {} (plan §12.2)",
                tombstoned,
                chunk.chunk_id
            );
        }
        if dropped > 0 {
            tracing::warn!(
                "extraction: {} malformed claim(s) dropped for {}",
                dropped,
                chunk.chunk_id
            );
        }

        // Disputes: the probe window is the valid target space (plan
        // §3.5), above the strength floor, canonical-ordered, with a
        // deterministic edge id so a re-extraction dedups.
        let neighbor_ids: HashSet<&str> =
            neighbors.iter().map(|n| n.chunk_id.as_str()).collect();
        let raw_disputes = parsed
            .get("disputes")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut disputes: Vec<DisputedEdge> = Vec::new();
        for raw in &raw_disputes {
            let cid = raw.get("chunk_id").and_then(|v| v.as_str()).unwrap_or("");
            if !neighbor_ids.contains(cid) {
                tracing::warn!(
                    "extraction: dispute targets non-probe chunk {cid}; ignored (plan §3.5)"
                );
                continue;
            }
            let strength = raw.get("strength").and_then(|v| v.as_f64()).filter(|s| s.is_finite())
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            if strength < self.config.dispute_strength_floor {
                tracing::warn!(
                    "extraction: dispute {}/{} at strength {} below floor {}; not opened",
                    chunk.chunk_id,
                    cid,
                    strength,
                    self.config.dispute_strength_floor
                );
                continue;
            }
            let (a, b) = (chunk.chunk_id.as_str(), cid);
            let (a, b) = if a <= b { (a, b) } else { (b, a) };
            let reason = raw.get("reason").and_then(|v| v.as_str()).unwrap_or("").to_string();
            disputes.push(DisputedEdge {
                edge_id: format!("d_{}", &sha256_hex(&format!("{a}|{b}"))[..24]),
                chunk_a: a.into(),
                chunk_b: b.into(),
                strength,
                opened_at: now.into(),
                closed_at: None,
                reason,
            });
        }

        // The chunk's decay profile (deterministic resolution).
        let (decay_class, expiry_naive) = resolve_decay_profile(&claims);
        let expiry: Option<String> =
            expiry_naive.map(|t| t.format(STORE_TIMESTAMP_FORMAT).to_string());

        // One transaction: disputes → propositions → support links →
        // terminal chunk write → watermark → confidence refresh.
        let chunk_id = chunk.chunk_id.clone();
        let source_entity = chunk.source_entity.clone();
        let source_reliability = chunk.source_reliability;
        let anchor_at = chunk.anchor_at.clone();
        let now_owned = now.to_string();
        let now_ref = now_naive;
        let audit_chunk_id = chunk_id.clone();
        let db = &self.db;
        let counts = db
            .run_in_transaction(
                || async move {
                    // Disputes first: the chunk's own dispute strength must
                    // include the edge it just opened when its propositions
                    // are scored.
                    let mut disputes_written = 0usize;
                    let mut new_endpoints: Vec<String> = Vec::new();
                    for e in &disputes {
                        if db.insert_disputed_edge(e).await? > 0 {
                            disputes_written += 1;
                            let other = if e.chunk_a == chunk_id {
                                e.chunk_b.clone()
                            } else {
                                e.chunk_a.clone()
                            };
                            new_endpoints.push(other);
                        }
                    }

                    let decay = match core::parse_utc(&anchor_at) {
                        Some(anchor) => core::decay(
                            &decay_class,
                            &anchor,
                            expiry_naive,
                            *now_ref,
                            &self.config,
                        )
                        .unwrap_or(0.0),
                        None => {
                            tracing::warn!(
                                "extraction: chunk {chunk_id} has an unparseable anchor_at; decay taken as 0"
                            );
                            0.0
                        }
                    };
                    let open = db.max_open_dispute_strength(&chunk_id).await?;
                    let weight =
                        core::effective_weight(source_reliability, core::dispute_strength(open));
                    let confidence = core::confidence(
                        self.config.correlation_lambda,
                        &[(source_entity, vec![weight * decay])],
                    );

                    let mut propositions_written = 0usize;
                    for c in &claims {
                        let prop = Proposition {
                            node_id: c.node_id.clone(),
                            claim: c.claim.clone(),
                            confidence,
                            is_disputed: open.is_some(),
                            status: "active".into(),
                            importance: c.importance.clone(),
                            urgency: c.urgency.clone(),
                            urgency_expires_at: expiry.clone(),
                            last_assessed_at: Some(now_owned.clone()),
                            archived_at: None,
                            created_at: now_owned.clone(),
                        };
                        propositions_written += db.insert_proposition(&prop).await? as usize;
                        db.add_support_link(&chunk_id, &c.node_id, &now_owned).await?;
                    }

                    db.finish_extraction(&chunk_id, &decay_class, expiry.as_deref())
                        .await?;
                    // Forward-only watermark (plan §3.6, MAX guard).
                    if let Some(rowid) = db.chunk_rowid(&chunk_id).await? {
                        db.advance_extraction_watermark(rowid).await?;
                    }
                    // A new edge changed both endpoints' effective weights —
                    // refresh every proposition they support (§8, same
                    // transaction as the cause).
                    let mut endpoints: Vec<&String> = vec![&chunk_id];
                    for e in new_endpoints.iter() {
                        endpoints.push(e);
                    }
                    for endpoint in endpoints {
                        for node in db.list_nodes_supported_by_chunk(endpoint).await? {
                            self.recompute_node_confidence(&node, now_ref).await?;
                        }
                    }
                    Ok((propositions_written, disputes_written))
                },
            )
            .await?;

        self.audit_extracted(now, &audit_chunk_id, counts.0, counts.1, false).await;
        Ok(ExtractedCounts {
            propositions: counts.0,
            disputes: counts.1,
            suppressed_skip: false,
        })
    }

    /// Local Neighborhood Probe (plan §3.5): never a full-DB scan — one
    /// bounded same-space vector pass (`k` = the active-chunk count, the
    /// brute-force cost §3.3 already pays per search) windowed in memory
    /// to `[conflict_probe_similarity_min, 0.98]`, excluding self.
    /// Cross-source chunks take priority — cross-source contradiction is
    /// the valuable signal — with same-source remainder filling the rest.
    async fn probe_neighbors(&self, chunk: &Chunk) -> Result<Vec<NeighborView>> {
        let (vec, tag) = self
            .embedder
            .embed(&chunk.content)
            .await
            .map_err(Error::Embed)?;
        let over_fetch = (self.db.count_active_chunks().await?).max(1) as usize;
        let hits = self
            .vectors
            .search(&vec, &tag, over_fetch)
            .await
            .map_err(Error::Internal)?;
        let min = self.config.conflict_probe_similarity_min;
        let limit = self.config.conflict_probe_neighbors as usize;
        let mut cross: Vec<Chunk> = Vec::new();
        let mut same: Vec<Chunk> = Vec::new();
        for (id, cos) in hits {
            if id == chunk.chunk_id {
                continue;
            }
            let c = cos as f64;
            if c < min || c > CONFLICT_PROBE_SIMILARITY_MAX {
                continue;
            }
            if let Some(ch) = self.db.get_chunk(&id).await? {
                if ch.status != "active" {
                    continue;
                }
                if ch.source_entity == chunk.source_entity {
                    same.push(ch);
                } else {
                    cross.push(ch);
                }
            }
        }
        let mut selected = cross;
        selected.truncate(limit);
        let fill = limit.saturating_sub(selected.len());
        let mut rest = same;
        rest.truncate(fill);
        selected.extend(rest);
        let mut views = Vec::new();
        for ch in selected {
            let claims = self
                .db
                .list_propositions_for_chunk(&ch.chunk_id)
                .await?
                .into_iter()
                .map(|p| format!("{} (importance: {})", p.claim, p.importance))
                .collect();
            views.push(NeighborView {
                chunk_id: ch.chunk_id,
                source_entity: ch.source_entity,
                content: ch.content,
                claims,
            });
        }
        Ok(views)
    }

    /// Re-derive a proposition's `confidence`/`is_disputed` strictly from
    /// its ACTIVE supporting chunks (plan §8) after a §9.1 edge event.
    /// Math from `core` only; archived supporters never enter. `pub(crate)`
    /// so the maintenance pass reuses it after archiving support.
    pub(crate) async fn recompute_node_confidence(
        &self,
        node_id: &str,
        now_naive: &NaiveDateTime,
    ) -> Result<()> {
        let db = &self.db;
        let cfg = &self.config;
        let supporters = db.list_active_supporting_chunks(node_id).await?;
        let mut by_source: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        let mut disputed = false;
        for ch in &supporters {
            let open = db.max_open_dispute_strength(&ch.chunk_id).await?;
            if open.is_some() {
                disputed = true;
            }
            let weight =
                core::effective_weight(ch.source_reliability, core::dispute_strength(open));
            let decay = match (
                core::parse_utc(&ch.anchor_at),
                ch.urgency_expires_at.as_deref().and_then(core::parse_utc),
            ) {
                (Some(anchor), exp) => {
                    core::decay(&ch.decay_class, &anchor, exp, *now_naive, cfg).unwrap_or(0.0)
                }
                _ => 0.0,
            };
            by_source.entry(ch.source_entity.clone()).or_default().push(weight * decay);
        }
        let entries: Vec<(String, Vec<f64>)> = by_source.into_iter().collect();
        let confidence = core::confidence(cfg.correlation_lambda, &entries);
        db.set_proposition_confidence(node_id, confidence, disputed).await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn audit_extracted(
        &self,
        now: &str,
        chunk_id: &str,
        propositions: usize,
        disputes: usize,
        suppressed: bool,
    ) {
        use crate::store::audit::AuditEntry;
        let mut detail = format!(
            "chunk={chunk_id} propositions={propositions} disputes={disputes}"
        );
        if suppressed {
            detail.push_str(" suppressed=1");
        }
        let entry = AuditEntry {
            id: uuid::Uuid::new_v4().to_string(),
            event: "extracted".into(),
            category: None,
            detail: Some(detail),
            created_at: now.into(),
        };
        if let Err(e) = self.db.insert_audit(&entry).await {
            tracing::warn!("audit_extracted: audit write failed ({e})");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_chunk(id: &str, source: &str) -> Chunk {
        Chunk {
            chunk_id: id.into(),
            content: format!("{source} sample content for extraction"),
            content_hash: sha256_hex(id),
            document_hash: sha256_hex(&format!("doc-{id}")),
            source_entity: source.into(),
            source_reliability: 0.5,
            provenance_cluster_id: "c_test".into(),
            cluster_citation: "Test: test".into(),
            status: "active".into(),
            extraction_status: "pending".into(),
            extraction_error_at: None,
            embedding_model: "__lexical_hash__".into(),
            decay_class: "transient".into(),
            anchor_at: "2026-08-01T00:00:00Z".into(),
            urgency_expires_at: None,
            reinforcement_count: 0,
            grounding_count: 0,
            archived_at: None,
            created_at: "2026-08-01T00:00:00Z".into(),
        }
    }

    fn claim(decay_class: &str, expiry: &str) -> Value {
        json!({
            "claim": "The meeting happens on Friday",
            "importance": decay_class,
            "urgency": "unknown",
            "decay_class": decay_class,
            "urgency_expires_at": expiry
        })
    }

    #[test]
    fn prompt_renders_conflicts_and_none() {
        let chunk = test_chunk("c_1", "srcA");
        let no_conflicts = build_extraction_prompt(&chunk, &[]);
        let user = no_conflicts[1]["content"].as_str().unwrap().to_string();
        assert!(user.contains("CHUNK (source_entity: srcA)"));
        assert!(user.contains("POTENTIAL_CONFLICTS:\n(none)"));

        let with_conflicts = build_extraction_prompt(
            &chunk,
            &[NeighborView {
                chunk_id: "c_2".into(),
                source_entity: "srcB".into(),
                content: "The meeting is on Monday".into(),
                claims: vec!["The meeting happens on Monday (importance: high)".into()],
            }],
        );
        let user2 = with_conflicts[1]["content"].as_str().unwrap().to_string();
        assert!(user2.contains("chunk_id: c_2 (source_entity: srcB)"));
        assert!(user2.contains("The meeting is on Monday"));
        assert!(user2.contains("(importance: high)"));
    }

    #[test]
    fn schema_is_total_grammar() {
        let schema = extraction_schema();
        let top: Vec<String> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(top, vec!["propositions", "disputes"]);
        let claim_required: Vec<String> = schema["properties"]["propositions"]["items"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            claim_required,
            vec![
                "claim", "importance", "urgency", "decay_class", "urgency_expires_at"
            ]
        );
        let dispute_required: Vec<String> = schema["properties"]["disputes"]["items"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(dispute_required, vec!["chunk_id", "strength", "reason"]);
    }

    #[test]
    fn profile_resolution_deadline_beats_static() {
        let mut claims: Vec<ValidClaim> = Vec::new();
        for (class, expiry) in [
            ("static", ""),
            ("deadline", "2026-09-01T00:00:00Z"),
            ("deadline", "2026-08-15T00:00:00Z"),
        ] {
            if let ClaimVerdict::Kept(c) =
                verify_claim(&claim(class, expiry), &HashSet::new())
            {
                claims.push(c);
            } else {
                panic!("claim should be kept");
            }
        }
        let (class, t) = resolve_decay_profile(&claims);
        assert_eq!(class, "deadline");
        // the EARLIEST expiry wins
        assert_eq!(
            t.unwrap().format(STORE_TIMESTAMP_FORMAT).to_string(),
            "2026-08-15T00:00:00Z"
        );
    }

    #[test]
    fn profile_resolution_static_beats_transient() {
        let c1 = match verify_claim(&claim("transient", ""), &HashSet::new()) {
            ClaimVerdict::Kept(c) => c,
            _ => panic!(),
        };
        let c2 = match verify_claim(&claim("static", ""), &HashSet::new()) {
            ClaimVerdict::Kept(c) => c,
            _ => panic!(),
        };
        let (class, t) = resolve_decay_profile(&[c1, c2]);
        assert_eq!(class, "static");
        assert!(t.is_none());
    }

    #[test]
    fn deadline_claim_without_valid_expiry_is_dropped() {
        let verdict = verify_claim(&claim("deadline", "not-a-date"), &HashSet::new());
        assert!(matches!(verdict, ClaimVerdict::Dropped));
        // the same expiry on a non-deadline claim is simply ignored
        let verdict = verify_claim(&claim("static", "not-a-date"), &HashSet::new());
        match verdict {
            ClaimVerdict::Kept(c) => assert!(c.urgency_expires_at.is_none()),
            _ => panic!("should keep"),
        }
    }

    #[test]
    fn tombstoned_claim_is_skipped() {
        let norm = normalize_text("The meeting happens on Friday");
        let tombstones = HashSet::from([sha256_hex(&norm)]);
        let verdict = verify_claim(&claim("static", ""), &tombstones);
        assert!(matches!(verdict, ClaimVerdict::Tombstoned));
    }
}
