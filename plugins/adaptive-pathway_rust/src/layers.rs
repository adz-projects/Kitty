//! The three belief layers and their decay half-lives. The half-life table is
//! the *entire* per-layer decay difference -- there are no other per-layer
//! code paths.

use crate::store::beliefs::Layer;

/// Half-life in days for each layer.
pub fn half_life_days(layer: Layer) -> f64 {
    match layer {
        Layer::Identity => 365.0,
        Layer::Context => 45.0,
        Layer::Conversation => 1.0,
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
