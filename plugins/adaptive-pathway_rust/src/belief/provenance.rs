//! Provenance → initial confidence and multiplicative reinforcement, ported
//! from the plan's recall section. Weak evidence structurally cannot carry a
//! belief fast.

use crate::store::beliefs::Provenance;
use crate::belief::EvidenceKind;

/// Initial confidence a newly-created belief starts at.
pub fn initial_confidence(p: Provenance) -> f64 {
    p.initial_confidence()
}

/// Reinforcement step by how decisive the evidence is.
pub fn reinforcement_step(evidence_kind: EvidenceKind) -> f64 {
    match evidence_kind {
        EvidenceKind::Correction => 0.60,
        EvidenceKind::DirectTest => 0.30,
        EvidenceKind::ControlledTest => 0.25,
        EvidenceKind::SupportiveObservation => 0.08,
        EvidenceKind::WeakObservation => 0.04,
    }
}

/// Multiplicative reinforcement toward the bound:
/// `c' = c + step·(1−c)` (positive) / `c' = c − step·c` (negative).
pub fn reinforce_toward(c: f64, positive: bool, step: f64) -> f64 {
    if positive {
        c + step * (1.0 - c)
    } else {
        c - step * c
    }
}

/// Untested ceiling: no belief may exceed this while untested.
pub const UNTESTED_CEILING: f64 = 0.75;

pub fn capped_untested(c: f64) -> f64 {
    c.min(UNTESTED_CEILING)
}

/// The ×0.625 untested discount reproduces 0.80 → 0.50 exactly.
pub fn untested_discount(confidence: f64) -> f64 {
    confidence * 0.625
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untested_discount_reproduces_0_8_to_0_5() {
        assert!((untested_discount(0.80) - 0.50).abs() < 1e-9);
    }

    #[test]
    fn initial_confidences_match_plan() {
        assert!((initial_confidence(Provenance::Correction) - 0.75).abs() < 1e-9);
        assert!((initial_confidence(Provenance::DirectStatement) - 0.70).abs() < 1e-9);
        assert!((initial_confidence(Provenance::ControlledTest) - 0.65).abs() < 1e-9);
        assert!((initial_confidence(Provenance::InferredPattern) - 0.30).abs() < 1e-9);
        assert!((initial_confidence(Provenance::SingleObservation) - 0.15).abs() < 1e-9);
    }

    #[test]
    fn weak_evidence_rises_slowly() {
        // 0.30 at step 0.08: c' = c + 0.08(1-c) -- several consistent
        // observations required to climb meaningfully, never an instant jump.
        let mut c = 0.30;
        for _ in 0..4 {
            c = reinforce_toward(c, true, 0.08);
        }
        // after 4 steps it's still clearly below the tested ceiling
        assert!(c < 0.55, "after 4 steps c={c}");
        // and it will eventually climb toward 1.0
        let mut c2 = 0.30;
        for _ in 0..60 {
            c2 = reinforce_toward(c2, true, 0.08);
        }
        assert!(c2 > 0.90, "after 60 steps c2={c2}");
        assert!(c2 < 1.0);
    }

    #[test]
    fn negative_reinforcement_decays() {
        let c = reinforce_toward(0.7, false, 0.30);
        assert!((c - 0.49).abs() < 1e-9);
    }
}
