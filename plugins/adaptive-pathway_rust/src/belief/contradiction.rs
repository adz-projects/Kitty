//! Contradiction detection: preserved, never silently resolved. Two triggers:
//! model-reported (via the schema's `contradicts` field, always trusted), and
//! engine-side cosine in [0.72, 0.93] with opposite mean polarity.

use crate::vector::ops;

/// Band thresholds for the engine-side similarity check. Above 0.93 → merge
/// instead; below 0.72 → unrelated; in between with opposite polarity →
/// contradiction.
pub const CONTRADICT_LOW: f64 = 0.72;
pub const CONTRADICT_HIGH: f64 = 0.93;

/// Does this cosine fall in the contradiction band?
pub fn in_contradiction_band(cos: f64) -> bool {
    (CONTRADICT_LOW..=CONTRADICT_HIGH).contains(&cos)
}

/// Does this cosine imply the two should merge instead (> 0.93)?
pub fn should_merge(cos: f64) -> bool {
    cos > CONTRADICT_HIGH
}

/// Mean signed polarity of an embedding's components (+1/−1 per component,
/// or 0.0 for a zero vector). Used as a cheap proxy for "meaning" direction
/// when comparing opposite claims.
pub fn mean_polarity(v: &[f32]) -> f64 {
    let n = v.len() as f64;
    if n == 0.0 {
        return 0.0;
    }
    v.iter().map(|&x| if x >= 0.0 { 1.0 } else { -1.0 }).sum::<f64>() / n
}

/// Do two beliefs form an engine-side contradiction? Requires cosine in the
/// band AND opposite mean polarity.
pub fn engine_contradiction(a: &[f32], b: &[f32]) -> bool {
    let cos = ops::cosine(a, b);
    in_contradiction_band(cos) && mean_polarity(a).signum() != mean_polarity(b).signum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_membership() {
        assert!(!in_contradiction_band(0.70));
        assert!(in_contradiction_band(0.85));
        assert!(!in_contradiction_band(0.95)); // merge instead
        assert!(should_merge(0.95));
    }

    #[test]
    fn polarity_signs() {
        assert!((mean_polarity(&[1.0, 1.0]) - 1.0).abs() < 1e-9);
        assert!((mean_polarity(&[-1.0, -1.0]) + 1.0).abs() < 1e-9);
        assert!((mean_polarity(&[1.0, -1.0])).abs() < 1e-9);
    }
}
