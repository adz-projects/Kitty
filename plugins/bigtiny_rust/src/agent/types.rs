/// Per-turn LLM connection metrics.
#[derive(Debug, Clone, Default)]
pub struct TimingResult {
    pub ttfb_ms: f64,
    pub ttft_ms: f64,
    pub generation_ms: f64,
    pub total_tokens: i32,
    /// Generation speed for *this* provider call.
    ///
    /// Computed here rather than left to the caller because this is the only
    /// place that holds both halves of the right fraction. The frontend used
    /// to divide the turn's accumulated output tokens by its own wall clock
    /// minus this struct's `ttft_ms` — two spans with different origins
    /// covering different work, so context building, memory recall, DB writes
    /// and every tool call in the turn all landed in the denominator while
    /// the numerator described only the last LLM call. On a tool-using turn
    /// with a reasoning model that understated the real rate roughly
    /// threefold.
    pub tokens_per_second: Option<f64>,
}

impl TimingResult {
    /// Fills in `tokens_per_second` from the fields already measured.
    ///
    /// `generation_ms - ttft_ms` is the decode window: time spent actually
    /// emitting tokens, excluding the wait for the first one (which is
    /// prompt processing and queueing, not generation). `None` when the
    /// window is degenerate or nothing was generated, so a caller never has
    /// to render a divide-by-zero.
    pub fn finalize_rate(&mut self) {
        let decode_ms = self.generation_ms - self.ttft_ms;
        self.tokens_per_second = (decode_ms > 0.0 && self.total_tokens > 0)
            .then(|| f64::from(self.total_tokens) / (decode_ms / 1000.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rate_uses_the_decode_window_not_the_whole_call() {
        // 100 tokens, 2s call, 1s of it waiting for the first token: 100 tok
        // over the 1s spent decoding, not over the full 2s.
        let mut t = TimingResult {
            ttft_ms: 1000.0,
            generation_ms: 2000.0,
            total_tokens: 100,
            ..Default::default()
        };
        t.finalize_rate();
        assert_eq!(t.tokens_per_second, Some(100.0));
    }

    #[test]
    fn a_degenerate_window_reports_nothing_rather_than_infinity() {
        for (ttft, generation, tokens) in [
            (2000.0, 2000.0, 100), // no decode window at all
            (3000.0, 2000.0, 100), // first token after the call "ended"
            (0.0, 1000.0, 0),      // nothing generated
        ] {
            let mut t = TimingResult {
                ttft_ms: ttft,
                generation_ms: generation,
                total_tokens: tokens,
                ..Default::default()
            };
            t.finalize_rate();
            assert_eq!(t.tokens_per_second, None, "{ttft}/{generation}/{tokens}");
        }
    }
}
