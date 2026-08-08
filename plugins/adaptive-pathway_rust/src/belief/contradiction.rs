//! Contradiction detection: preserved, never silently resolved. Two triggers:
//! model-reported (via the schema's `contradicts` field, always trusted), and
//! engine-side cosine in [0.72, 0.93] with opposite mean polarity.

use crate::vector::ops;

/// Band thresholds for the engine-side similarity check. Above 0.93 → merge
/// instead; below 0.72 → unrelated; in between with opposite polarity →
/// contradiction.
pub const CONTRADICT_LOW: f64 = 0.72;
pub const CONTRADICT_HIGH: f64 = 0.93;

/// Below this `|mean_polarity|` a vector has no meaningful dominant
/// direction — `0.0f64.signum()` is `+1.0`, so a perfectly balanced vector
/// used to read as "positive polarity" and could be flagged as contradicting
/// a genuinely negative one. Anything this flat is *neutral*: it has no
/// polarity to oppose, so it never forms an engine-side contradiction.
const POLARITY_EPS: f64 = 0.1;

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

/// A sign with a neutral zone: returns `None` when the vector is too
/// balanced to have a dominant polarity (see `POLARITY_EPS`) — a neutral
/// vector is trivially compatible with both polarities and must not be able
/// to trip the "opposite polarity" contradiction check.
fn polarity_sign(p: f64) -> Option<f64> {
    if p.abs() < POLARITY_EPS {
        None
    } else {
        Some(p.signum())
    }
}

/// Do two beliefs form an engine-side contradiction? Requires cosine in the
/// band AND opposite mean polarity, with neither vector polarity-neutral.
pub fn engine_contradiction(a: &[f32], b: &[f32]) -> bool {
    let cos = ops::cosine(a, b);
    if !in_contradiction_band(cos) {
        return false;
    }
    match (polarity_sign(mean_polarity(a)), polarity_sign(mean_polarity(b))) {
        (Some(pa), Some(pb)) => pa != pb,
        _ => false,
    }
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
