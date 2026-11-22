//! The three belief layers and their decay half-lives.

use crate::store::beliefs::{BeliefKind, Layer};

/// Half-life in days for each layer (legacy/default).
pub fn half_life_days(layer: Layer) -> f64 {
    match layer {
        Layer::Identity => 365.0,
        Layer::Context => 45.0,
        Layer::Conversation => 1.0,
    }
}

/// Half-life in days by `(layer, kind)`: transient task/project context
/// (`Layer::Context + BeliefKind::Context`) decays in 10 days instead of 45,
/// while enduring preferences, stances, and traits keep the 45-day half-life.
pub fn half_life_days_for(layer: Layer, kind: BeliefKind) -> f64 {
    match (layer, kind) {
        (Layer::Context, BeliefKind::Context) => 10.0,
        _ => half_life_days(layer),
    }
}

/// Multiplicative decay factor for delta-days under this layer's half-life:
/// `exp(-Δdays / half_life)`. `days` is quantized to whole days (so the
/// top-6 ordering can't flip mid-day and churn the block).
pub fn decay_factor(layer: Layer, days_ago: i64) -> f64 {
    if days_ago <= 0 {
        return 1.0;
    }
    (-(days_ago as f64) / half_life_days(layer)).exp()
}

/// Multiplicative decay factor taking [`BeliefKind`] into account.
pub fn decay_factor_for(layer: Layer, kind: BeliefKind, days_ago: i64) -> f64 {
    if days_ago <= 0 {
        return 1.0;
    }
    (-(days_ago as f64) / half_life_days_for(layer, kind)).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_kind_decays_faster_than_preference_in_context_layer() {
        let task_ctx = decay_factor_for(Layer::Context, BeliefKind::Context, 20);
        let pref_ctx = decay_factor_for(Layer::Context, BeliefKind::Preference, 20);
        assert!(
            task_ctx < pref_ctx,
            "transient Context kind ({task_ctx:.3}) must decay faster than Preference ({pref_ctx:.3})"
        );
    }
}
