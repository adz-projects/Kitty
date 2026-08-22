use once_cell::sync::Lazy;
use serde_json::Value;

/// Real tokenizer, not a heuristic — `tiktoken-rs` was already a declared
/// dependency (chosen specifically for accurate, fast token counting) but
/// was never actually wired up; this module used a bare `bytes / 4`
/// estimate instead. That estimate is systematically wrong for anything
/// that isn't plain English prose (CJK text, code, JSON, dense punctuation),
/// and every consumer of this count is a real budget: compaction
/// thresholds, the live-tail cap, and the emergency-trim valve that's
/// supposed to guarantee a request never exceeds the model's actual context
/// window. An inaccurate count there isn't just imprecise bookkeeping — it
/// can under-trim right up to (or past) a real API-level context-length
/// error, which the trimming logic exists specifically to prevent.
static ENCODING: Lazy<Option<tiktoken_rs::CoreBPE>> = Lazy::new(|| tiktoken_rs::cl100k_base().ok());

/// Count tokens for plain text content using the real `cl100k_base`
/// tokenizer (falls back to the old byte-based heuristic only if the
/// embedded encoding data somehow fails to load, which should never happen
/// in practice — better a rough estimate than a panic).
pub fn count_text_tokens(text: &str) -> i32 {
    if text.is_empty() {
        return 0;
    }
    match ENCODING.as_ref() {
        Some(enc) => enc.encode_ordinary(text).len() as i32,
        None => text.len() as i32 / 4,
    }
}

/// Token count for one context message, matching what actually gets serialized.
pub fn count_message_tokens(msg: &Value) -> i32 {
    let mut total = 0;

    if let Some(content) = msg.get("content") {
        if let Some(text) = content.as_str() {
            total += count_text_tokens(text);
        } else if let Some(blocks) = content.as_array() {
            for block in blocks {
                if let Some(block) = block.as_object() {
                    if let Some(block_type) = block.get("type").and_then(|t| t.as_str()) {
                        if block_type == "text" {
                            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                total += count_text_tokens(text);
                            }
                        } else if block_type == "image" {
                            total += 256;
                        }
                    }
                }
            }
        }
    }

    if let Some(tool_calls) = msg.get("tool_calls") {
        total += count_text_tokens(&serde_json::to_string(tool_calls).unwrap_or_default());
    }

    if let Some(tool_call_id) = msg.get("tool_call_id") {
        total += count_text_tokens(&tool_call_id.to_string());
    }

    // Small fixed overhead per message for role/framing tokens
    total + 4
}

/// Token count for a list of messages.
pub fn count_messages_tokens(messages: &[Value]) -> i32 {
    messages.iter().map(count_message_tokens).sum()
}

// ---------------------------------------------------------------------------
// Wrap-up valve budgeting
//
// The tool loop re-sends its whole grown history on every iteration, and
// nothing between `ContextBuilder::build_messages` (once, before the loop) and
// the provider's own 400 was watching how big it got. These four functions are
// that watch. They are pure and live here — rather than inline in
// `run_tool_loop` — for the same reason `exceeds_content_ceiling` was factored
// out of `process_stream`: this arithmetic sits at a cliff edge, and it has to
// be testable without standing up an `AgentLoop`.
// ---------------------------------------------------------------------------

/// How much room the wrap-up valve keeps in reserve: `min(window * ratio, cap)`.
///
/// `min` rather than `max` is load-bearing. The ratio binds on small windows (a
/// 32k model reserves 8k, not an unusable-in-practice 15k that would fire on
/// ordinary work) and the cap binds on large ones (a 1M model reserves 15k, not
/// 250k). With the defaults the two arms cross at a 60k window.
///
/// Every input is clamped rather than trusted: `context_length` is whatever a
/// provider reported or a user typed, and `ratio` can come from hand-edited
/// YAML. A ratio above 1.0 would make the reserve exceed the window, so
/// `wrapup_due` would be true on every turn at step 0 and the agent could never
/// call a tool at all — silent bricking, so it is clamped here as well as in
/// `TokenManagementConfig::sanitize`. Both should hold independently.
pub fn context_reserve_tokens(context_length: i32, ratio: f64, cap: i32) -> i32 {
    let window = context_length.max(0);
    let ratio = if ratio.is_nan() {
        DEFAULT_WRAPUP_RESERVE_RATIO
    } else {
        ratio.clamp(0.0, 1.0)
    };
    let cap = cap.max(0);
    let from_ratio = (window as f64 * ratio).floor();
    // The f64 -> i32 cast saturates in Rust, so a huge window cannot wrap.
    (from_ratio as i32).min(cap).clamp(0, window)
}

/// The ratio `context_reserve_tokens` falls back to when handed a NaN. Kept
/// beside the function rather than imported from `config` so this module has no
/// dependency on config load order.
const DEFAULT_WRAPUP_RESERVE_RATIO: f64 = 0.25;

/// Best estimate of what the *next* request will cost as input.
///
/// Prefers the provider's own `input_tokens` from the last completed response,
/// plus a local count of everything appended since. Counting the message array
/// alone systematically **undercounts** what the provider bills: it cannot see
/// the tool schemas (`tools_for_turn` is routinely 2-6k tokens with a normal
/// MCP tool set), the provider's system framing, or Anthropic's tool-result
/// regrouping. A reserve built on the array alone fires late — which is exactly
/// the failure the valve exists to prevent.
///
/// It is also much cheaper. Counting the whole array means a full cl100k encode
/// of the entire transcript on every loop iteration, inside an `async fn` with
/// no `.await` in the region — tens of milliseconds of blocked Tokio worker per
/// step on a long session. The delta form encodes one assistant message and its
/// tool results.
///
/// `mark` is clamped to the slice length: the step-budget branch pops its
/// injected system message *after* the mark is taken, so a mark past the end is
/// legitimate rather than a bug. The few tokens that pop leaves uncounted are an
/// overcount in the safe direction.
pub fn projected_input_tokens(last_usage: Option<(usize, i32)>, messages: &[Value]) -> i32 {
    match last_usage {
        None => count_messages_tokens(messages),
        Some((mark, reported)) => {
            let mark = mark.min(messages.len());
            reported.saturating_add(count_messages_tokens(&messages[mark..]))
        }
    }
}

/// True once remaining room has fallen below the reserve.
///
/// Saturating, because `used_tokens` can legitimately exceed `context_length` —
/// the context builder budgets against the daemon-wide `max_context_tokens`
/// while this checks the provider's own window, so a provider with a smaller
/// real window starts the turn already over. That case must answer `true`, not
/// wrap to a large positive.
pub fn wrapup_due(used_tokens: i32, context_length: i32, reserve: i32) -> bool {
    context_length.saturating_sub(used_tokens) < reserve
}

/// Output budget for the wrap-up request: enough for a closing paragraph, never
/// enough to push `input + max_tokens` past the window.
///
/// The result must always land in `1..=65536`. Anthropic requires a positive
/// integer, and `anthropic.rs` filters an explicit max to that range and
/// *silently discards* anything outside it — which would hand control back to
/// the 4096 default and, because the wrap-up request carries no tools,
/// re-enable extended thinking. A zero or negative here is not a smaller
/// budget; it is a much larger one.
pub fn wrapup_max_tokens(context_length: i32, used_tokens: i32, floor: i32, ceiling: i32) -> i32 {
    let floor = floor.max(1);
    let ceiling = ceiling.max(floor);
    context_length
        .saturating_sub(used_tokens)
        .clamp(floor, ceiling)
}

#[cfg(test)]
mod wrapup_tests {
    use super::*;
    use serde_json::json;

    const RATIO: f64 = 0.25;
    const CAP: i32 = 15_000;

    /// The whole point of `min`: the ratio governs small windows and the cap
    /// governs large ones. With the defaults they cross at exactly 60k, so
    /// that row is the one that pins the operator choice.
    #[test]
    fn reserve_crosses_from_ratio_to_cap_at_a_sixty_thousand_window() {
        for (window, expected) in [
            (8_192, 2_048),    // small local model — ratio binds
            (32_768, 8_192),   // ratio binds; a flat 15k here would be absurd
            (60_000, 15_000),  // the knee: both arms agree
            (64_000, 15_000),  // the daemon's own max_context_tokens default
            (200_000, 15_000), // Claude-class — the cap is what stops a 50k reserve
            (1_000_000, 15_000),
        ] {
            assert_eq!(
                context_reserve_tokens(window, RATIO, CAP),
                expected,
                "window {window}"
            );
        }
    }

    #[test]
    fn reserve_never_exceeds_the_window_and_never_goes_negative() {
        // Degenerate windows from a provider that reports nonsense.
        assert_eq!(context_reserve_tokens(0, RATIO, CAP), 0);
        assert_eq!(context_reserve_tokens(-1, RATIO, CAP), 0);
        // A ratio above 1.0 would otherwise reserve more than the whole
        // window, making every turn wrap up at step 0 — the agent could never
        // call a tool again.
        let r = context_reserve_tokens(10_000, 2.0, CAP);
        assert!(r <= 10_000, "reserve {r} must not exceed the window");
        // NaN falls back to the default rather than poisoning the comparison.
        assert_eq!(
            context_reserve_tokens(40_000, f64::NAN, CAP),
            context_reserve_tokens(40_000, RATIO, CAP)
        );
        assert_eq!(context_reserve_tokens(40_000, -0.5, CAP), 0);
        assert_eq!(context_reserve_tokens(40_000, RATIO, -100), 0);
    }

    /// Pins the inclusive/exclusive boundary. With a 64k window and a 15k
    /// reserve the tipping point is 49,000 used: at exactly that figure the
    /// remaining room *equals* the reserve and the turn continues.
    #[test]
    fn wrapup_due_boundary_is_exclusive() {
        assert!(!wrapup_due(48_999, 64_000, 15_000));
        assert!(!wrapup_due(49_000, 64_000, 15_000));
        assert!(wrapup_due(49_001, 64_000, 15_000));
    }

    /// Reachable whenever the provider's real window is smaller than the
    /// daemon-wide budget the context builder assembled against. Must answer
    /// "yes, wrap up" rather than wrapping around to a large positive.
    #[test]
    fn an_overshoot_past_the_window_still_reports_due() {
        assert!(wrapup_due(70_000, 64_000, 15_000));
        assert!(wrapup_due(i32::MAX, 64_000, 15_000));
    }

    #[test]
    fn projected_input_prefers_provider_usage_plus_the_delta() {
        let msgs: Vec<Value> = (0..5)
            .map(|i| json!({"role": "user", "content": format!("message number {i}")}))
            .collect();

        // No usage reported yet (step 0, or a provider that reports none).
        assert_eq!(
            projected_input_tokens(None, &msgs),
            count_messages_tokens(&msgs)
        );

        // Marked at 3: the provider's own number, plus only what followed.
        let got = projected_input_tokens(Some((3, 50_000)), &msgs);
        assert_eq!(got, 50_000 + count_messages_tokens(&msgs[3..]));
        assert!(got > 50_000, "the delta must actually be added");

        // Marked at the end: nothing appended since, so exactly the report.
        assert_eq!(projected_input_tokens(Some((5, 50_000)), &msgs), 50_000);
    }

    /// The step-budget branch pops its injected system message *after* the
    /// usage mark is taken, so a mark beyond the current length is a normal
    /// occurrence — and an unclamped slice index here is a panic in
    /// production, on a code path that only runs once a session is already in
    /// trouble.
    #[test]
    fn a_mark_past_the_end_clamps_instead_of_panicking() {
        let msgs: Vec<Value> = (0..5)
            .map(|i| json!({"role": "user", "content": format!("m{i}")}))
            .collect();
        assert_eq!(projected_input_tokens(Some((9, 50_000)), &msgs), 50_000);
        assert_eq!(projected_input_tokens(Some((9, 0)), &[]), 0);
    }

    #[test]
    fn projected_input_saturates_rather_than_overflowing() {
        let msgs = vec![json!({"role": "user", "content": "some content here"})];
        assert_eq!(projected_input_tokens(Some((0, i32::MAX)), &msgs), i32::MAX);
    }

    #[test]
    fn wrapup_max_tokens_stays_in_the_range_anthropic_will_accept() {
        const FLOOR: i32 = 512;
        const CEILING: i32 = 2_048;

        // The common case: the valve fires with reserve-sized room left, so
        // the ceiling binds.
        assert_eq!(wrapup_max_tokens(64_000, 50_000, FLOOR, CEILING), 2_048);
        // The middle band that is actually reachable near the cliff.
        assert_eq!(wrapup_max_tokens(64_000, 63_000, FLOOR, CEILING), 1_000);
        assert_eq!(wrapup_max_tokens(64_000, 63_800, FLOOR, CEILING), FLOOR);

        // Overshoot: the answer must still be a positive integer. Zero or
        // negative is not a smaller budget — `anthropic.rs` discards an
        // out-of-range max, reverting to 4096 *and* re-enabling thinking.
        let over = wrapup_max_tokens(64_000, 70_000, FLOOR, CEILING);
        assert_eq!(over, FLOOR);
        assert!(over >= 1);

        for used in [0, 1_000, 63_999, 64_000, 200_000, i32::MAX] {
            let v = wrapup_max_tokens(64_000, used, FLOOR, CEILING);
            assert!(
                (1..=65_536).contains(&v),
                "max_tokens {v} for used {used} would be silently discarded"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_count_text_tokens_empty() {
        assert_eq!(count_text_tokens(""), 0);
    }

    #[test]
    fn test_count_text_tokens_basic() {
        // Real cl100k_base encoding of this phrase is 6 tokens (each word
        // happens to be its own token) — coincidentally the same number the
        // old `bytes/4` heuristic produced for this particular string, but
        // this now reflects the actual tokenizer, not an approximation.
        let tokens = count_text_tokens("Hello world this is a test");
        assert_eq!(tokens, 6);
    }

    #[test]
    fn test_count_text_tokens_cjk_is_not_underestimated() {
        // The old `bytes/4` heuristic significantly undercounts CJK text:
        // 3 bytes/char / 4 ≈ 0.75 "tokens" per character, while cl100k_base
        // typically spends close to one full token per CJK character —
        // real encoding must come out noticeably higher than the old
        // heuristic would have, not lower.
        let text = "你好世界这是一个测试"; // 10 CJK characters, 30 bytes
        let byte_heuristic = text.len() as i32 / 4; // what the old code returned
        let real = count_text_tokens(text);
        assert!(
            real > byte_heuristic,
            "expected real tokenizer ({real}) to exceed the old byte-based heuristic ({byte_heuristic})"
        );
    }

    #[test]
    fn test_count_message_tokens_simple() {
        let msg = json!({
            "role": "user",
            "content": "Hello"
        });
        let tokens = count_message_tokens(&msg);
        // "Hello" = 1 real token + 4 fixed per-message overhead = 5
        assert!(tokens >= 4);
    }

    #[test]
    fn test_count_message_tokens_with_tool_calls() {
        let msg = json!({
            "role": "assistant",
            "content": "Let me check",
            "tool_calls": [
                {
                    "id": "call-1",
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"path\": \"/test.txt\"}"
                    }
                }
            ]
        });
        let tokens = count_message_tokens(&msg);
        // Should include content + tool_calls JSON + overhead
        assert!(tokens > 8);
    }

    #[test]
    fn test_count_messages_tokens() {
        let messages = vec![
            json!({"role": "user", "content": "Hello"}),
            json!({"role": "assistant", "content": "Hi there"}),
        ];
        let total = count_messages_tokens(&messages);
        assert!(total > 0);
    }
}
