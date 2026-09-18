//! Core computation (plan §15 Phase 5): numeric pins for §7.1 decay, §7.2
//! earliest deadline, §8.1/§8.2 effective weight and confidence, §10.1
//! activation, §10.2 priority, and the §5.3/§6.1/§7.3/§7.4 lifecycle
//! predicates.
//!
//! The pure math module owns no I/O: every pin here feeds it plain values
//! (timestamps as store-format strings) and checks the frozen formulas
//! bit-for-bit where the spec fixes them.

use chrono::{Duration, NaiveDateTime};

use memorabilia::config::Config;
use memorabilia::core::{
    can_promote, confidence, decay, deadline_elapsed, dispute_strength, earliest_deadline,
    effective_weight, hop, metaboost, parse_utc, priority, seed_activation, should_archive_chunk,
    should_prune, source_weight, unsupported_transition, urgency_is_unknown,
};

const ANCHOR: &str = "2026-08-01T00:00:00Z";
const T_EXP: &str = "2026-09-01T00:00:00Z";
const LAMBDA: f64 = 0.05;

fn dt(s: &str) -> NaiveDateTime {
    parse_utc(s).unwrap()
}

fn d(days: i64) -> Duration {
    Duration::days(days)
}

fn approx(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-12,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn parse_utc_accepts_store_format_and_rejects_the_rest() {
    let v = parse_utc("2026-08-20T10:00:00Z").unwrap();
    assert_eq!(v.to_string(), "2026-08-20 10:00:00");
    assert!(parse_utc("2026-08-20T10:00:00").is_none());
    assert!(parse_utc("2026-08-20 10:00:00Z").is_none());
    assert!(parse_utc("garbage").is_none());
}

#[test]
fn decay_static_pinned_on_tau() {
    let cfg = Config::default();
    let a = dt(ANCHOR);

    assert_eq!(decay("static", &a, None, a, &cfg), Some(1.0));
    approx(decay("static", &a, None, a + d(365), &cfg).unwrap(), (-1.0f64).exp());
    approx(decay("static", &a, None, a + d(730), &cfg).unwrap(), (-2.0f64).exp());

    let one_day = decay("static", &a, None, a + d(1), &cfg).unwrap();
    assert!(1.0 > one_day && one_day > decay("static", &a, None, a + d(365), &cfg).unwrap());

    // τ is a knob, not a constant: with tau_static_days = 100 the curve
    // falls to e^-1 after 100 days.
    let mut cfg = Config::default();
    cfg.tau_static_days = 100;
    approx(decay("static", &a, None, a + d(100), &cfg).unwrap(), (-1.0f64).exp());
}

#[test]
fn decay_transient_pinned_on_tau() {
    let cfg = Config::default();
    let a = dt(ANCHOR);

    assert_eq!(decay("transient", &a, None, a, &cfg), Some(1.0));
    approx(decay("transient", &a, None, a + d(7), &cfg).unwrap(), (-1.0f64).exp());
    approx(decay("transient", &a, None, a + d(3), &cfg).unwrap(), (-3.0f64 / 7.0).exp());
    approx(decay("transient", &a, None, a + d(35), &cfg).unwrap(), (-5.0f64).exp());
    assert!(decay("transient", &a, None, a + d(2), &cfg).unwrap()
        > decay("transient", &a, None, a + d(3), &cfg).unwrap());
}

#[test]
fn decay_deadline_rises_to_one_then_decays() {
    let cfg = Config::default();
    let t = dt(T_EXP);

    // Rises to 1.0 as the deadline looms (salience ramp, tau_pre = 7d).
    approx(
        decay("deadline", &t, Some(t), t - d(14), &cfg).unwrap(),
        0.5 + 0.5 * (-2.0f64).exp(),
    );
    assert!(
        (decay("deadline", &t, Some(t), t - d(14), &cfg).unwrap() - 0.56766764161830635).abs()
            < 1e-9
    );
    approx(
        decay("deadline", &t, Some(t), t - d(7), &cfg).unwrap(),
        0.5 + 0.5 * (-1.0f64).exp(),
    );
    assert!(
        (decay("deadline", &t, Some(t), t - d(7), &cfg).unwrap() - 0.68393972058572116).abs()
            < 1e-9
    );
    assert_eq!(decay("deadline", &t, Some(t), t, &cfg), Some(1.0));
    assert!(
        decay("deadline", &t, Some(t), t - d(14), &cfg).unwrap()
            < decay("deadline", &t, Some(t), t - d(7), &cfg).unwrap()
    );
    assert!(
        decay("deadline", &t, Some(t), t - d(14), &cfg).unwrap() < 1.0
    );

    // Post-deadline incident window (tau_post = 48h = 2d).
    approx(decay("deadline", &t, Some(t), t + d(1), &cfg).unwrap(), (-0.5f64).exp());
    approx(decay("deadline", &t, Some(t), t + d(2), &cfg).unwrap(), (-1.0f64).exp());
    assert!(
        decay("deadline", &t, Some(t), t + d(1), &cfg).unwrap()
            > decay("deadline", &t, Some(t), t + d(2), &cfg).unwrap()
    );

    // Far before the deadline the curve sits at its 0.5 asymptote.
    assert!((decay("deadline", &t, Some(t), t - d(365), &cfg).unwrap() - 0.5).abs() < 1e-6);

    // The curve is continuous at T: both branches evaluate to 1.0 there,
    // and the value never leaves [0, 1] anywhere along the sweep.
    for off in (-40..=40).step_by(3) {
        let v = decay("deadline", &t, Some(t), t + d(off), &cfg).unwrap();
        assert!(v >= 0.0 && v <= 1.0, "decay {v} out of [0,1] at offset {off}d");
    }

    // τ knobs: with tau_pre = 14d the T−14d point is one ramp-e-time out;
    // with tau_post = 24h the T+24h point is one window-e-time out.
    let mut cfg = Config::default();
    cfg.tau_pre_days = 14;
    approx(
        decay("deadline", &t, Some(t), t - d(14), &cfg).unwrap(),
        0.5 + 0.5 * (-1.0f64).exp(),
    );
    let mut cfg = Config::default();
    cfg.tau_post_hours = 24;
    approx(decay("deadline", &t, Some(t), t + d(1), &cfg).unwrap(), (-1.0f64).exp());
}

#[test]
fn decay_rejects_unknown_class_and_missing_expiry() {
    let cfg = Config::default();
    let a = dt(ANCHOR);
    let t = dt(T_EXP);
    assert!(decay("wobbly", &a, None, a, &cfg).is_none());
    assert!(decay("deadline", &a, None, t, &cfg).is_none());
}

#[test]
fn earliest_deadline_is_the_min_over_active_chunks() {
    let t = dt(T_EXP);
    assert_eq!(earliest_deadline(&[]), None);
    assert_eq!(earliest_deadline(&[t + d(5)]), Some(t + d(5)));
    let expiries = vec![t + d(10), t, t + d(3)];
    assert_eq!(earliest_deadline(&expiries), Some(t));
}

#[test]
fn dispute_strength_and_effective_weight() {
    assert_eq!(dispute_strength(None), 0.0);
    assert_eq!(dispute_strength(Some(0.7)), 0.7);
    assert_eq!(dispute_strength(Some(1.5)), 1.0);
    assert_eq!(dispute_strength(Some(-0.2)), 0.0);

    approx(effective_weight(0.9, 0.0), 0.9);
    assert_eq!(effective_weight(0.9, 1.0), 0.0);
    approx(effective_weight(0.9, 0.8), 0.18);
    assert!(effective_weight(-0.5, 0.0) >= 0.0);
    assert!(effective_weight(0.9, 0.0) <= 1.0);
}

#[test]
fn source_weight_max_plus_count_term_bounded() {
    // N = 1: the log term is log(1) = 0, so a lone chunk keeps its weight.
    assert_eq!(source_weight(1.0, &[0.9]), 0.9);

    // N = 2: max (not sum) + λ·log(2).
    approx(source_weight(LAMBDA, &[0.9, 0.6]), 0.9 + LAMBDA * 2.0f64.ln());

    // Bounded to 1 − 1e-6 no matter how large λ or the weights are.
    approx(source_weight(10.0, &[0.999, 0.999, 0.999, 0.999]), 1.0 - 1e-6);

    assert_eq!(source_weight(LAMBDA, &[]), 0.0);
}

#[test]
fn confidence_pinned_over_distinct_sources() {
    // Empty support: zero confidence (lifecycle owns the fate, §7.4).
    assert_eq!(confidence(LAMBDA, &[]), 0.0);

    // One source, one chunk: confidence is the chunk's discounted weight.
    approx(confidence(LAMBDA, &[("a".into(), vec![0.9])]), 0.9);

    // One source, two chunks: max + λ·log(2) — the second chunk nudges.
    approx(confidence(LAMBDA, &[("a".into(), vec![0.9, 0.85])]), 0.9 + LAMBDA * 2.0f64.ln());

    // Two independent sources each beat a correlated pair from one origin.
    let correlated = confidence(LAMBDA, &[("a".into(), vec![0.9, 0.9])]);
    let independent = confidence(
        LAMBDA,
        &[("a".into(), vec![0.9]), ("b".into(), vec![0.9])],
    );
    approx(independent, 0.99);
    assert!(independent > correlated);

    // Entries for the same source_entity fold into one W_S — passing the
    // same origin as two separate entries (two clusters, one origin) must
    // not multiply or otherwise change the result.
    let split = confidence(LAMBDA, &[("a".into(), vec![0.9]), ("a".into(), vec![0.85])]);
    let whole = confidence(LAMBDA, &[("a".into(), vec![0.9, 0.85])]);
    assert_eq!(split, whole);

    // More distinct sources strictly increases confidence.
    let one = confidence(LAMBDA, &[("a".into(), vec![0.5])]);
    let two = confidence(LAMBDA, &[("a".into(), vec![0.5]), ("b".into(), vec![0.5])]);
    let three = confidence(
        LAMBDA,
        &[("a".into(), vec![0.5]), ("b".into(), vec![0.5]), ("c".into(), vec![0.5])],
    );
    assert!(one < two && two < three);
}

#[test]
fn disputed_edge_lowers_confidence_and_archive_of_disputer_restores_it() {
    let lambda = Config::default().correlation_lambda;

    // A proposition whose single supporting chunk (reliability 0.9, full
    // decay → w_effective·decay = 0.9) carries an open DISPUTED edge at
    // strength 0.8 contributes 0.9 · (1 − 0.8) = 0.18 instead.
    let w = effective_weight(0.9, dispute_strength(None));
    let before = confidence(lambda, &[("a".into(), vec![w])]);

    let w_disputed = effective_weight(0.9, dispute_strength(Some(0.8)));
    let during = confidence(lambda, &[("a".into(), vec![w_disputed])]);
    assert!(during < before, "open DISPUTED edge must lower confidence");
    approx(during, 0.18);

    // The disputer archives → its open edge closes (§9.1) → the chunk's
    // dispute strength is 0.0 again and confidence restores exactly.
    let after = confidence(lambda, &[("a".into(), vec![w])]);
    assert_eq!(before, after);

    // Same invariant when the disputed chunk is not the source max:
    // discounting it lowers the source weight; restoring it restores the
    // max.
    let clean = confidence(
        lambda,
        &[("a".into(), vec![0.95, 0.6])],
    );
    let hot = effective_weight(0.95, dispute_strength(Some(1.0)));
    let contested = confidence(lambda, &[("a".into(), vec![hot, 0.6])]);
    let restored = confidence(
        lambda,
        &[("a".into(), vec![effective_weight(0.95, dispute_strength(None)), 0.6])],
    );
    assert!(contested < clean);
    assert_eq!(restored, clean);
}

#[test]
fn seed_activation_is_max_clamped_not_sum() {
    approx(seed_activation(&[0.9, 0.8]), 0.9);
    // Near-duplicate evidence counts once.
    approx(seed_activation(&[0.9, 0.9, 0.9]), 0.9);
    // Negative cosines clamp to 0.
    approx(seed_activation(&[-0.2, 0.4]), 0.4);
    assert_eq!(seed_activation(&[-0.5]), 0.0);
    assert_eq!(seed_activation(&[]), 0.0);
}

#[test]
fn hop_decays_activating_paths_and_pruning_is_strict() {
    // Weights ≤ 0.8 and γ = 0.5 keep activation in [0,1] hop over hop.
    let a1 = 1.0;
    let a2 = hop(a1, 0.8, 0.5);
    let a3 = hop(a2, 0.8, 0.5);
    let a4 = hop(a3, 0.8, 0.5);
    approx(a2, 0.4);
    approx(a3, 0.16);
    approx(a4, 0.064);
    assert!((0.0..=1.0).contains(&a4));
    assert!(hop(a4, 0.8, 0.5) < 0.05 && !should_prune(a4, 0.05));
    assert_eq!(hop(1.0, 1.0, 1.0), 1.0);

    assert!(!should_prune(0.05, 0.05));
    assert!(should_prune(0.0499, 0.05));
    assert!(!should_prune(0.051, 0.05));
}

#[test]
fn metaboost_components_pinned_per_level() {
    let cfg = Config::default();

    approx(metaboost("low", "low", &cfg), 1.2);
    approx(metaboost("medium", "medium", &cfg), 1.7);
    approx(metaboost("high", "high", &cfg), 2.8);
    // Unknown importance pays the triage premium, not an importance boost.
    approx(metaboost("high", "unknown", &cfg), 2.5);
    // Unknown urgency contributes nothing.
    approx(metaboost("unknown", "low", &cfg), 1.1);
    approx(metaboost("unknown", "unknown", &cfg), 1.5);

    // The cap is a real boundary: raised knobs push the raw sum to 4.0,
    // which must clamp to metaboost_cap = 3.0.
    let mut cfg = Config::default();
    cfg.metaboost_urgency_high = 1.5;
    cfg.metaboost_importance_high = 1.5;
    approx(metaboost("high", "high", &cfg), 3.0);

    // Knob wiring: zeroing a component removes exactly its addend.
    let mut cfg = Config::default();
    cfg.metaboost_urgency_high = 0.0;
    approx(metaboost("high", "low", &cfg), 1.1);
    let mut cfg = Config::default();
    cfg.metaboost_unknown_triage_premium = 0.0;
    approx(metaboost("high", "unknown", &cfg), 2.0);
}

#[test]
fn priority_is_multiplicative_bounded_and_deterministically_orderable() {
    approx(priority(0.5, 2.8, 3.0), 1.4);
    approx(priority(1.0, 3.0, 3.0), 3.0);
    assert_eq!(priority(0.0, 3.0, 3.0), 0.0);
    approx(priority(1.0, 4.0, 3.0), 3.0);
    // Out-of-range inputs clamp into [0, cap].
    approx(priority(1.5, 2.0, 3.0), 2.0);
    assert_eq!(priority(-0.3, 2.0, 3.0), 0.0);

    // Ordering is deterministic across repeated calls, whatever the input
    // order: the sorted priority sequence must be identical.
    let items: Vec<(f64, f64)> = vec![
        (0.9, 2.8),
        (0.5, 2.0),
        (0.77, 3.0),
        (0.5, 2.8),
        (0.9, 1.2),
        (0.2, 3.0),
        (0.77, 1.1),
    ];
    let ranked = |input: &[(f64, f64)]| -> Vec<f64> {
        let mut scores: Vec<f64> = input
            .iter()
            .map(|&(a, b)| priority(a, b, 3.0))
            .collect();
        scores.sort_by(|x, y| y.partial_cmp(x).unwrap());
        scores
    };
    let first = ranked(&items);
    let reversed: Vec<(f64, f64)> = items.iter().rev().copied().collect();
    assert_eq!(first, ranked(&reversed));
    assert_eq!(first, ranked(&items));
}

#[test]
fn chunk_archive_predicates() {
    let cfg = Config::default();
    let now = dt(ANCHOR);
    let t = dt(T_EXP);

    // Decay-out: strictly below the prune threshold.
    assert!(should_archive_chunk("static", 0.04, 0.05, None, now, &cfg));
    assert!(!should_archive_chunk("static", 0.05, 0.05, None, now, &cfg));
    assert!(!should_archive_chunk("static", 0.06, 0.05, None, now, &cfg));

    // Deadline archive fires past T + τ_post even while decay (e^-1 ≈ 0.37)
    // is far above the prune threshold — and not one second early.
    let decay_at_deadline = decay("deadline", &t, Some(t), t + d(2), &cfg).unwrap();
    assert!(decay_at_deadline > 0.05);
    assert!(should_archive_chunk(
        "deadline",
        decay_at_deadline,
        0.05,
        Some(t),
        t + d(2) + Duration::seconds(1),
        &cfg
    ));
    assert!(!should_archive_chunk("deadline", decay_at_deadline, 0.05, Some(t), t + d(2), &cfg));
    assert!(!should_archive_chunk("deadline", decay_at_deadline, 0.05, Some(t), t + d(1), &cfg));

    // Non-deadline classes never archive on wall-clock expiry.
    assert!(!should_archive_chunk("static", 0.9, 0.05, Some(t), t + d(30), &cfg));

    // §7.3 urgency transition shares the same boundary.
    assert!(!deadline_elapsed(&t, &(t + d(2)), &cfg));
    assert!(deadline_elapsed(&t, &(t + d(2) + Duration::seconds(1)), &cfg));
    assert!(urgency_is_unknown(Some(&t), &(t + d(2) + Duration::seconds(1)), &cfg));
    assert!(!urgency_is_unknown(Some(&t), &(t + d(2)), &cfg));
    assert!(!urgency_is_unknown(None, &(t + d(30)), &cfg));
}

#[test]
fn unsupported_transition_archives_high_and_unknown_only() {
    assert_eq!(unsupported_transition("high"), "UNSUPPORTED_ARCHIVE");
    assert_eq!(unsupported_transition("unknown"), "UNSUPPORTED_ARCHIVE");
    assert_eq!(unsupported_transition("medium"), "DELETE");
    assert_eq!(unsupported_transition("low"), "DELETE");
}

#[test]
fn each_promotion_gate_blocks_individually() {
    let cfg = Config::default();

    let gates = |count: i64,
                 disputed: bool,
                 reliability: f64,
                 sources: i64,
                 controlled: bool|
     -> bool {
        can_promote(count, disputed, reliability, sources, controlled, &cfg)
    };

    // All four gates pass.
    assert!(gates(5, false, 0.7, 2, false));
    // Gate 1: below the reinforcement threshold.
    assert!(!gates(4, false, 0.7, 2, false));
    // Gate 1 boundary: exactly the threshold passes; saturation beyond it
    // does not un-promote.
    assert!(gates(5, false, 0.7, 2, false));
    assert!(gates(7, false, 0.7, 2, false));
    // Gate 2: an open DISPUTED edge blocks.
    assert!(!gates(5, true, 0.7, 2, false));
    // Gate 3: reliability below the floor blocks; exactly at it passes.
    assert!(!gates(5, false, 0.59, 2, false));
    assert!(gates(5, false, 0.6, 2, false));
    // Gate 4: single origin without controlled-test provenance blocks;
    // controlled-test provenance substitutes for a second origin.
    assert!(!gates(5, false, 0.7, 1, false));
    assert!(gates(5, false, 0.7, 1, true));
    assert!(gates(5, false, 0.7, 3, false));
}
