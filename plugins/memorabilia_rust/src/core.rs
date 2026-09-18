//! Core computation (plan §15 Phase 5): the pure math behind decay,
//! confidence, activation, priority, and lifecycle transitions.
//!
//! I/O-free by contract: no SQL, no async, no clocks. Timestamps arrive as
//! the store's ISO-8601 UTC `…Z` strings (parsed via [`parse_utc`]) and all
//! other inputs are plain values, so Phases 6–8 compose these functions
//! around their store queries instead of the other way round.
//!
//! Functions are total on their documented domain: unknown categories
//! (an unrecognized `decay_class`, an unparseable timestamp) yield `None`
//! or zero rather than panicking — the soft-fail discipline lives in the
//! callers, and this module must not be a panic source under live data.

use chrono::NaiveDateTime;
use std::collections::BTreeMap;

use crate::config::Config;

/// Seconds in a day — a unit constant for the τ (days) ↔ timestamp math,
/// named rather than bare per the no-magic-literals rule.
const SECONDS_PER_DAY: f64 = 86_400.0;
/// Upper bound of a per-source weight `W_S` (plan §8.2: `1 − 10⁻⁶`), so a
/// single source can never reach exactly 1.0 confidence on its own.
const SOURCE_WEIGHT_EPSILON: f64 = 1e-6;
/// Pre-deadline salience base of the §7.1 piecewise curve: decay sits at
/// `1 − 0.5` far before the deadline and ramps to exactly 1.0 at
/// `T_expire`. Part of the frozen formula, named for the same reason as
/// `SECONDS_PER_DAY`.
const DEADLINE_SALIENCE_BASE: f64 = 0.5;

/// Parse a store-format UTC timestamp (`"2026-08-20T10:00:00Z"`). A bare
/// `DateTime::parse_from_str` would fail on this shape (no explicit offset
/// in the string); the `Naive` form is the parse for UTC-second columns.
pub fn parse_utc(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ").ok()
}

/// Signed day distance `to − from` (fractional).
fn days_between(from: &NaiveDateTime, to: &NaiveDateTime) -> f64 {
    (*to - *from).num_seconds() as f64 / SECONDS_PER_DAY
}

/// `Decay(C, t) ∈ [0,1]` for all three classes (plan §7.1), with the τ
/// constants read from `cfg`:
///
/// * `static` / `transient`: `e^−(t − t_anchor)/τ` — the two classes differ
///   only in τ (and the promotion path that switches between them).
/// * `deadline`: piecewise around `T_expire` with a pre-deadline salience
///   ramp `0.5 + 0.5·e^−(T − t)/τ_pre` (the curve *rises* to 1.0 as the
///   deadline looms) and a post-deadline incident window
///   `e^−(t − T)/τ_post`.
///
/// `None` for an unrecognized class or a `deadline` chunk without
/// `T_expire` (the CHECK constraint normally rules both out).
pub fn decay(
    decay_class: &str,
    anchor_at: &NaiveDateTime,
    t_expire: Option<NaiveDateTime>,
    now: NaiveDateTime,
    cfg: &Config,
) -> Option<f64> {
    match decay_class {
        "static" => {
            let tau = cfg.tau_static_days as f64;
            Some((-(days_between(anchor_at, &now)) / tau).exp().clamp(0.0, 1.0))
        }
        "transient" => {
            let tau = cfg.tau_transient_days as f64;
            Some((-(days_between(anchor_at, &now)) / tau).exp().clamp(0.0, 1.0))
        }
        "deadline" => {
            let t = t_expire?;
            let tau_pre = cfg.tau_pre_days as f64;
            let tau_post = cfg.tau_post_hours as f64 / 24.0;
            let d = days_between(&t, &now);
            let v = if d <= 0.0 {
                DEADLINE_SALIENCE_BASE
                    + DEADLINE_SALIENCE_BASE * (d / tau_pre).exp()
            } else {
                (-d / tau_post).exp()
            };
            Some(v.clamp(0.0, 1.0))
        }
        _ => None,
    }
}

/// Earliest-deadline rule (plan §7.2): `T_expire(P) = min(T_expire(C_i))`
/// over the active supporting chunks' expiries. Archived chunks drop out of
/// the `min` because the caller only passes active rows.
pub fn earliest_deadline(expiries: &[NaiveDateTime]) -> Option<NaiveDateTime> {
    expiries.iter().copied().min()
}

/// True once the post-deadline incident window has fully elapsed:
/// `now > T_expire + τ_post` (plan §7.3 / §11 per-tick deadline
/// resolution).
pub fn deadline_elapsed(t_expire: &NaiveDateTime, now: &NaiveDateTime, cfg: &Config) -> bool {
    let tau_post_days = cfg.tau_post_hours as f64 / 24.0;
    days_between(t_expire, now) > tau_post_days
}

/// §7.3: `urgency` transitions `HIGH → UNKNOWN` exactly when
/// [`deadline_elapsed`] does; non-deadline propositions never do.
pub fn urgency_is_unknown(
    t_expire: Option<&NaiveDateTime>,
    now: &NaiveDateTime,
    cfg: &Config,
) -> bool {
    match t_expire {
        Some(t) => deadline_elapsed(t, now, cfg),
        None => false,
    }
}

/// §5.3 archive predicate, math half: a chunk archives when it has decayed
/// strictly below the prune threshold, or (deadline class) once the
/// incident window has elapsed. Supersession-by-newer-evidence is a
/// judgment the extraction/maintenance phases own, not this formula.
pub fn should_archive_chunk(
    decay_class: &str,
    decay: f64,
    prune_threshold: f64,
    t_expire: Option<NaiveDateTime>,
    now: NaiveDateTime,
    cfg: &Config,
) -> bool {
    if decay < prune_threshold {
        return true;
    }
    if decay_class == "deadline" {
        if let Some(t) = t_expire {
            return deadline_elapsed(&t, &now, cfg);
        }
    }
    false
}

/// `dispute_strength` of a chunk (plan §8): the maximum strength of its
/// OPEN `DISPUTED` edges, `0.0` when disputed-free. Closed edges impose no
/// dispute (§9.1: archived materials drop out of dispute).
pub fn dispute_strength(max_open_edge_strength: Option<f64>) -> f64 {
    max_open_edge_strength.unwrap_or(0.0).clamp(0.0, 1.0)
}

/// `w_effective = source_reliability × (1 − dispute_strength)` (plan §8.1) —
/// the single channel by which a contradiction lowers confidence.
pub fn effective_weight(source_reliability: f64, dispute_strength: f64) -> f64 {
    (source_reliability * (1.0 - dispute_strength)).clamp(0.0, 1.0)
}

/// Per-source weight `W_S` (plan §8.2): the strongest discounted signal of
/// source `S` (`max` — correlated clusters collapse to their best chunk,
/// never multiply) plus the diminishing-returns count term
/// `λ·log(1 + |N_S − 1|)`, bounded to `1 − 10⁻⁶`. `effective_decays` are
/// the chunk-level `w_effective · Decay(C, t)` values for `S`'s active
/// chunks; empty → `0.0`.
pub fn source_weight(lambda: f64, effective_decays: &[f64]) -> f64 {
    let n = effective_decays.len();
    if n == 0 {
        return 0.0;
    }
    let max_term = effective_decays.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let raw = max_term + lambda * (n as f64).ln();
    raw.min(1.0 - SOURCE_WEIGHT_EPSILON)
}

/// `Confidence(P) = 1 − ∏_S (1 − W_S)` over DISTINCT `source_entity` values
/// (plan §8.2). `sources` maps each entry's key to that origin's chunk-level
/// `w_effective · decay` values; entries sharing a key are folded into one
/// `W_S`, so two clusters from the same origin cannot double-count.
/// Deterministic (BTreeMap fold order); empty → `0.0`.
pub fn confidence(lambda: f64, sources: &[(String, Vec<f64>)]) -> f64 {
    let mut by_source: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    for (source, weights) in sources {
        by_source.entry(source.as_str()).or_default().extend(weights.iter().copied());
    }
    let mut surviving = 1.0;
    for weights in by_source.values() {
        surviving *= 1.0 - source_weight(lambda, weights);
    }
    (1.0 - surviving).clamp(0.0, 1.0)
}

/// Seed activation = `max(CosSim)` over the retrieved supporting chunks,
/// clamped to [0,1] (plan §10.1: max, **not** sum — near-duplicate evidence
/// must not double-count). Empty → `0.0`.
pub fn seed_activation(similarities: &[f64]) -> f64 {
    similarities
        .iter()
        .cloned()
        .fold(f64::NEG_INFINITY, f64::max)
        .clamp(0.0, 1.0)
}

/// One spreading-activation hop: multiply the incoming activation by the
/// traversed edge weight and the per-hop decay γ (plan §10.1). With
/// weights ≤ 0.8 and γ ≤ 1 the result stays in [0,1]; the clamp is
/// defensive for out-of-range inputs.
pub fn hop(activation: f64, edge_weight: f64, gamma: f64) -> f64 {
    (activation * edge_weight * gamma).clamp(0.0, 1.0)
}

/// Prune the path: accumulated activation strictly below δ (plan §10.1).
pub fn should_prune(activation: f64, threshold: f64) -> bool {
    activation < threshold
}

/// `MetaBoost(V) = min(1 + UrgencyBoost + ImportanceBoost +
/// UnknownTriagePremium, metaboost_cap)` (plan §10.2). Each level outside
/// high/medium/low contributes 0 (including `urgency == "unknown"`, which
/// post-deadline §7.3 is a *decay* of salience, not a boost);
/// `importance` outside the four CHECK-enforced levels is treated as
/// unknown and takes the triage premium.
pub fn metaboost(urgency: &str, importance: &str, cfg: &Config) -> f64 {
    let urgency_boost = match urgency {
        "high" => cfg.metaboost_urgency_high,
        "medium" => cfg.metaboost_urgency_medium,
        "low" => cfg.metaboost_urgency_low,
        _ => 0.0,
    };
    let importance_boost = match importance {
        "high" => cfg.metaboost_importance_high,
        "medium" => cfg.metaboost_importance_medium,
        "low" => cfg.metaboost_importance_low,
        _ => cfg.metaboost_unknown_triage_premium,
    };
    (1.0 + urgency_boost + importance_boost).min(cfg.metaboost_cap)
}

/// `Priority(V) = Activation(V) · MetaBoost(V)` (plan §10.2): multiplicative
/// so the bounded similarity (∈ [0,1]) and bounded multiplier (∈ [1,3])
/// combine in one common scale, result in `[0, cap]`.
pub fn priority(activation: f64, boost: f64, cap: f64) -> f64 {
    (activation.clamp(0.0, 1.0) * boost).clamp(0.0, cap)
}

/// §7.4 explicit-deletion rule for a proposition whose active support fell
/// to `N_active = 0`: `importance ∈ {high, unknown}` archive to
/// `UNSUPPORTED_ARCHIVE` (forensic); medium/low delete outright.
pub fn unsupported_transition(importance: &str) -> &'static str {
    match importance {
        "high" | "unknown" => "UNSUPPORTED_ARCHIVE",
        _ => "DELETE",
    }
}

/// §6.1 promotion gates (transient → static): all four must hold —
/// `reinforcement_count ≥` threshold, no open DISPUTED edge,
/// `source_reliability ≥` floor, and independent corroboration (≥ 2
/// distinct `source_entity` origins or `controlled_test` provenance).
/// Frequency alone never promotes.
pub fn can_promote(
    reinforcement_count: i64,
    open_dispute: bool,
    source_reliability: f64,
    distinct_sources: i64,
    controlled_test: bool,
    cfg: &Config,
) -> bool {
    reinforcement_count >= cfg.reinforcement_promote_threshold as i64
        && !open_dispute
        && source_reliability >= cfg.promotion_min_reliability
        && (distinct_sources >= 2 || controlled_test)
}
