/// Per-turn LLM connection metrics.
#[derive(Debug, Clone, Default)]
pub struct TimingResult {
    pub ttfb_ms: f64,
    pub ttft_ms: f64,
    pub generation_ms: f64,
    pub total_tokens: i32,
}
