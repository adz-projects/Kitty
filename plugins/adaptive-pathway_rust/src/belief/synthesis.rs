//! Merging/upserting an extracted observation into the belief store.
//!
//! On a new observation: embed, then route to an existing belief (cosine ≥
//! 0.86 merge: bump support/sessions, recompute confidence, take max
//! provenance, re-parent observations) or create a new context/conversation
//! belief; record the observation and flag an assumption if warranted;
//! detect engine-side contradictions. All in one transaction.

use chrono::{DateTime, Utc};
use serde_json::json;

use super::super::store::beliefs::{Belief, Layer, Provenance, BeliefPatch};
use super::super::store::Db;
use crate::error::Result;
use crate::belief;

/// Cosine above which an observation routes into an existing belief (merge)
/// rather than starting a new one.
pub const MERGE_COSINE: f64 = 0.86;

/// Route and record a single new observation, inside one `BEGIN`/`COMMIT`
/// transaction (`Db::run_in_transaction`) -- a crash or error between, say,
/// `update_belief` bumping `support_count` and the matching
/// `insert_observation` used to leave the belief's counters permanently out
/// of sync with its own audit trail. Delegates to `route_observation_inner`,
/// which does the actual work and is what `Db::run_in_transaction`'s
/// closure calls.
#[allow(clippy::too_many_arguments)]
pub async fn route_observation(
    db: &Db,
    statement: &str,
    embedding: &[f32],
    embedding_model: &str,
    provenance: Provenance,
    layer: Layer,
    domain: Option<&str>,
    evidence: Option<&str>,
    contradicts: Option<&str>,
    session_id: Option<String>,
    batch_id: Option<&str>,
    now: DateTime<Utc>,
) -> Result<()> {
    db.run_in_transaction(move || {
        route_observation_inner(
            db, statement, embedding, embedding_model, provenance, layer, domain, evidence,
            contradicts, session_id, batch_id, now,
        )
    })
    .await
}

#[allow(clippy::too_many_arguments)]
async fn route_observation_inner(
    db: &Db,
    statement: &str,
    embedding: &[f32],
    embedding_model: &str,
    provenance: Provenance,
    layer: Layer,
    domain: Option<&str>,
    evidence: Option<&str>,
    contradicts: Option<&str>,
    session_id: Option<String>,
    batch_id: Option<&str>,
    now: DateTime<Utc>,
) -> Result<()> {
    // Guard against relearning a forgotten/suppressed statement (text-hash).
    let text_hash = text_hash(statement);
    if db.has_permanent_tombstone(&text_hash).await? {
        return Ok(());
    }

    // Find best merge candidate in the same layer. Conversation-layer
    // beliefs are session-scoped ("lives for the session") -- merging must
    // never pull in another session's still-fast-decaying conversational
    // memory, so that search is narrowed to this session specifically.
    // Context/identity beliefs are cross-session by design and use the
    // unscoped list. Also excludes any candidate still tagged with a stale
    // `embedding_model` -- `embedding` was just computed under
    // `embedding_model` (the current one), so comparing it against a
    // stale-space candidate via cosine would be the same meaningless
    // cross-space comparison `list_recall_candidates` guards against.
    let candidates: Vec<_> = match (layer, session_id.as_deref()) {
        (Layer::Conversation, Some(sid)) => db.list_conversation_beliefs_for_session(sid).await?,
        _ => db.list_beliefs(Some(layer)).await?,
    }
    .into_iter()
    .filter(|b| b.embedding_model == embedding_model)
    .collect();
    let mut best: Option<(String, f64)> = None;
    for b in candidates.iter() {
        let cos = crate::vector::ops::cosine(&b.embedding, embedding);
        if cos >= MERGE_COSINE
            && best.as_ref().map(|(_, c)| cos > *c).unwrap_or(true) {
                best = Some((b.id.clone(), cos));
            }
    }

    let id = crate::store::audit::uuid_string();

    if let Some((target_id, _cos)) = &best {
        let target = db.get_belief(target_id.as_str()).await?.unwrap_or_else(|| {
            // shouldn't happen; create a fallback belief
            fallback_belief(&id, statement, embedding, embedding_model, provenance, layer, domain, session_id.as_deref(), now)
        });
        // Merge: bump support/sessions, recompute confidence, take max
        // provenance ranking, re-parent this observation onto target.
        let new_conf = merge_confidence(target.confidence, provenance);
        let new_prov = max_provenance(target.provenance, provenance);
        let session_is_new = match &session_id {
            Some(sid) => target.session_id.as_deref() != Some(sid.as_str()),
            None => false,
        };
        let target_id = target.id.clone();
        db.update_belief(
            &target.id,
            &BeliefPatch {
                confidence: Some(new_conf),
                support_count: Some(target.support_count + 1),
                distinct_sessions: Some(target.distinct_sessions + if session_is_new { 1 } else { 0 }),
                // The merged-in evidence's provenance is carried onto the
                // belief itself (not just the observation row) -- otherwise
                // a belief formed as `single_observation` that later gets
                // reinforced by a `direct_statement` merge would keep
                // reporting the weak original provenance forever, which both
                // the promotion gate and the untested recall discount read
                // directly off this field.
                provenance: Some(new_prov),
                tested: Some(target.tested || new_prov.is_tested()),
                last_confirmed_at: Some(now),
                ..Default::default()
            },
            now,
        )
        .await?;
        db.insert_observation(&crate::store::observations::Observation {
            id,
            belief_id: Some(target.id),
            session_id: session_id.clone(),
            statement: statement.to_string(),
            provenance: new_prov.as_str().to_string(),
            layer: layer.as_str().to_string(),
            domain: domain.map(|s| s.to_string()),
            evidence: evidence.map(|s| s.to_string()),
            contradicts: contradicts.map(|s| s.to_string()),
            created_at: now,
            batch_id: batch_id.map(|s| s.to_string()),
        })
        .await?;

        // Merging is inherently supporting (the observation routed here
        // *because* it's similar, cosine >= MERGE_COSINE) -- decisive
        // provenance on a merge resolves any live assumption tracking this
        // belief as passed. The `wrong`-reason `forget` path is what
        // resolves an assumption as failed; there's no "this merge
        // contradicts its own target" case to handle here. Best-effort:
        // never let assumption bookkeeping fail the whole observation.
        if new_prov.is_tested() {
            let _ = db.resolve_assumption_for_belief(&target_id, true).await;
        }
        if let Some(updated) = db.get_belief(&target_id).await.ok().flatten() {
            let current_exchange = db.global_exchange_count().await.unwrap_or(0);
            let _ = db.flag_assumption_if_warranted(&updated, current_exchange).await;
        }
    } else {
        // Create a new belief.
        let b = fallback_belief(&id, statement, embedding, embedding_model, provenance, layer, domain, session_id.as_deref(), now);
        db.insert_belief(&b).await?;
        // A brand-new belief's initial confidence never clears the 0.55
        // flag threshold while untested (the highest untested initial
        // confidence, inferred_pattern, starts at 0.30) -- this can only
        // ever flag after a future confidence-tuning change, but the check
        // is cheap and idempotent, so it's here for that eventuality rather
        // than being silently skipped on this branch.
        let current_exchange = db.global_exchange_count().await.unwrap_or(0);
        let _ = db.flag_assumption_if_warranted(&b, current_exchange).await;
        db.insert_observation(&crate::store::observations::Observation {
            id: crate::store::audit::uuid_string(),
            belief_id: Some(id),
            session_id: session_id.clone(),
            statement: statement.to_string(),
            provenance: provenance.as_str().to_string(),
            layer: layer.as_str().to_string(),
            domain: domain.map(|s| s.to_string()),
            evidence: evidence.map(|s| s.to_string()),
            contradicts: contradicts.map(|s| s.to_string()),
            created_at: now,
            batch_id: batch_id.map(|s| s.to_string()),
        })
        .await?;
    }

    // Detection: model-reported contradiction via the `contradicts` field.
    // The field carries a *statement* the model says the new observation
    // contradicts; best-effort resolve it to a belief id and record an open
    // contradiction. Never fatal.
    if let Some(other_stmt) = contradicts {
        let current_id = best.as_ref().map(|(i, _)| i.clone()).unwrap_or_default();
        let resolved = db
            .best_text_match(other_stmt)
            .await
            .unwrap_or_default();
        if let Some(other_id) = resolved {
            if other_id != current_id {
                let _ = db
                    .insert_contradiction(&crate::store::contradictions::Contradiction {
                        id: crate::store::audit::uuid_string(),
                        belief_a: current_id,
                        belief_b: other_id,
                        status: "open".into(),
                        resolved_b: None,
                        reason: Some("model_reported".into()),
                        created_at: now,
                        resolved_at: None,
                    })
                    .await;
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn fallback_belief(
    id: &str,
    statement: &str,
    embedding: &[f32],
    embedding_model: &str,
    provenance: Provenance,
    layer: Layer,
    domain: Option<&str>,
    session_id: Option<&str>,
    now: DateTime<Utc>,
) -> Belief {
    Belief {
        id: id.to_string(),
        text: statement.to_string(),
        embedding: embedding.to_vec(),
        embedding_model: embedding_model.to_string(),
        confidence: belief::provenance::initial_confidence(provenance),
        provenance,
        layer,
        tested: false,
        domain: domain.map(|s| s.to_string()),
        tier: "conversation".into(),
        support_count: 1,
        distinct_sessions: 1,
        contradict_count: 0,
        pinned: false,
        last_confirmed_at: Some(now),
        consolidated_at: None,
        created_at: now,
        updated_at: now,
        // Context/identity beliefs are cross-session by design even if a
        // session_id happened to be passed in; only conversation-layer
        // beliefs are session-owned.
        session_id: if layer == Layer::Conversation {
            session_id.map(|s| s.to_string())
        } else {
            None
        },
    }
}

/// Recompute confidence on a merge. Multiplicative toward the bound with a
/// modest step for a supportive observation.
pub fn merge_confidence(existing: f64, provenance: Provenance) -> f64 {
    let step = belief::provenance::reinforcement_step(
        if provenance == Provenance::Correction {
            belief::EvidenceKind::Correction
        } else {
            belief::EvidenceKind::SupportiveObservation
        },
    );
    belief::provenance::reinforce_toward(existing, true, step)
}

/// Higher provenance wins (correction > direct_statement > controlled_test >
/// inferred_pattern > single_observation).
pub fn max_provenance(a: Provenance, b: Provenance) -> Provenance {
    use Provenance::*;
    let rank = |p: Provenance| match p {
        Correction => 5,
        DirectStatement => 4,
        ControlledTest => 3,
        InferredPattern => 2,
        SingleObservation => 1,
    };
    if rank(b) > rank(a) {
        b
    } else {
        a
    }
}

/// Deterministic text hash (mmh3 over the statement), used for tombstones and
/// suppression keys.
pub fn text_hash(text: &str) -> String {
    format!("{:x}", crate::embed::hashing::mmh3_32(text.as_bytes(), 0))
}

/// Tell the model-tool pair about a fresh observation (JSON export helper).
pub fn observation_json(statement: &str, layer: &str, confidence: f64) -> serde_json::Value {
    json!({
        "statement": statement,
        "layer": layer,
        "confidence": confidence,
    })
}
