pub mod contradiction;
pub mod lifecycle;
pub mod provenance;
pub mod synthesis;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::layers;
use crate::store::beliefs::Belief;

/// The top-level block rendered from recall. Fields map 1:1 to the four
/// recall sections plus the universal footer.
#[derive(Debug, Clone, Serialize)]
pub struct RecallBlock {
    pub section: String,
    pub content: String,
}

/// A single selected belief with its computed effective weight, used for the
/// `[What I know about you]` block (≤6 beliefs, DPP-selected).
#[derive(Debug, Clone)]
pub struct SelectedBelief {
    pub belief: Belief,
    pub effective_weight: f64,
}

/// A candidate for the `[Worth testing]` slot: a scheduled assumption.
#[derive(Debug, Clone, Serialize)]
pub struct WorthTesting {
    pub text: String,
}

/// A line for `[Where I'm unsure]` / `[Check yourself]`.
#[derive(Debug, Clone, Serialize)]
pub struct UncertaintyLine {
    pub text: String,
}

/// Compute the effective recall weight of a belief at recall time. Never
/// stored -- always recomputed from the stored fields plus the *now* it's
/// called with.
pub fn effective_weight(
    b: &Belief,
    query_domain: Option<&str>,
    now: DateTime<Utc>,
) -> f64 {
    // base: tested -> confidence; untested -> confidence * 0.625 (so 0.80
    // becomes exactly 0.50).
    let base = if b.tested {
        b.confidence
    } else {
        b.confidence * 0.625
    };

    let dm = crate::domains::domain_match(crate::domains::same_domain(
        query_domain,
        b.domain.as_deref(),
    ));

    let days: i64 = b
        .last_confirmed_at
        .map(|t| (now - b.updated_at.max(t)).num_days())
        .unwrap_or_else(|| (now - b.updated_at).num_days());
    let dec = layers::decay_factor(b.layer, days);

    let contradicted = if b.contradict_count > 0 { 0.5 } else { 1.0 };

    let mut w = base * dm * dec * contradicted;

    // pinned always floors the effective weight at 0.8
    if b.pinned {
        w = w.max(0.8);
    }
    // suppressed is handled upstream by filtering (we never store it here),
    // matching the plan: `w = if suppressed { 0.0 }`.
    w
}

/// Remove beliefs whose text-hash is actively suppressed. `effective_weight`
/// itself has no DB access and can't check this per-belief, so filtering
/// happens upstream, before any scoring -- matching the comment in
/// `effective_weight` that says suppression "is handled upstream by
/// filtering" (it previously wasn't handled anywhere at all). Suppression is
/// looked up by text-hash, not id, since a belief re-created with the same
/// text after a merge would otherwise slip back past an id-keyed check.
pub fn filter_suppressed(
    beliefs: Vec<Belief>,
    suppressed_hashes: &std::collections::HashSet<String>,
) -> Vec<Belief> {
    beliefs
        .into_iter()
        .filter(|b| !suppressed_hashes.contains(&crate::belief::synthesis::text_hash(&b.text)))
        .collect()
}

/// Provenance-to-initial-confidence mapping and the reinforcement step sizes,
/// ported from `embeddings/decision` semantics; see `provenance.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceKind {
    Correction,
    DirectTest,
    ControlledTest,
    SupportiveObservation,
    WeakObservation,
}
