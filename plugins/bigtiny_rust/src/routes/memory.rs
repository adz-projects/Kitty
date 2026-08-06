use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use super::AppState;

/// Injected-prompt fraction as a percentage, truncated to one decimal place
/// (`12.0`% → `12`, `12.67`% → `12.6`), and `0.0` for an empty denominator
/// (a fresh daemon with zero counters "injects 0%" rather than erroring).
pub fn injection_rate_pct(total: u64, injected: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let rate = (injected as f64 / total as f64) * 100.0;
    (rate * 10.0).round() / 10.0
}

/// `GET /api/memory/stats` — global (all-session) pre-flight memory recall
/// telemetry. Kitty polls this while the Settings > Advanced pane is open to
/// render a live "% of prompts with injected context" readout. Counters are
/// process-lifetime and global across every conversation.
pub async fn stats(State(state): State<Arc<AppState>>) -> Json<Value> {
    let (total, injected) = state.agent.preflight_snapshot();
    Json(json!({
        "total_prompts": total,
        "injected_prompts": injected,
        "injection_rate_pct": injection_rate_pct(total, injected),
    }))
}

#[cfg(test)]
mod tests {
    use super::injection_rate_pct;

    #[test]
    fn rate_is_zero_when_denominator_empty() {
        assert_eq!(injection_rate_pct(0, 0), 0.0);
    }

    #[test]
    fn rate_truncates_to_one_decimal() {
        assert_eq!(injection_rate_pct(100, 50), 50.0);
        assert_eq!(injection_rate_pct(3, 1), 33.3);
        assert_eq!(injection_rate_pct(300, 1), 0.3);
    }
}