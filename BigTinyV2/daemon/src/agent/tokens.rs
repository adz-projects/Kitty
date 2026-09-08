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

/// The private key carrying a message's already-known token count.
///
/// Underscore-prefixed and stripped before any request leaves the daemon (see
/// `provider::wire::sanitize_for_wire`).
pub const TOKEN_HINT_KEY: &str = "_tok";

/// Whether reuse is enabled. A kill switch, not a tuning knob.
///
/// This code decides whether a request fits the context window, and a subtle
/// error surfaces as an opaque provider 400 rather than anything legible. If a
/// budget anomaly ever appears in the field, flipping this to `false` restores
/// V1's always-recount behaviour without a rebuild being the only option.
static REUSE_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_token_reuse(enabled: bool) {
    REUSE_ENABLED.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

pub fn token_reuse_enabled() -> bool {
    REUSE_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Stamp a known token count onto a message.
///
/// Called where a message is built from a row whose `token_count` was computed
/// by this same function at save time, so the stored number is what a recount
/// would produce -- not an estimate of it.
pub fn stamp_token_hint(msg: &mut Value, tokens: i32) {
    if let Some(obj) = msg.as_object_mut() {
        obj.insert(TOKEN_HINT_KEY.to_string(), Value::from(tokens));
    }
}

/// Remove the hint, forcing a recount.
///
/// **Every transform that mutates a message's content must call this.** The
/// list is short and enumerable -- tool masking, the image-block collapse,
/// image normalization, tool-call stringification, thought-seed injection --
/// and the failure mode of forgetting one is a lost optimization, not a wrong
/// budget: a stripped message simply recounts, which is what V1 always did.
pub fn clear_token_hint(msg: &mut Value) {
    if let Some(obj) = msg.as_object_mut() {
        obj.remove(TOKEN_HINT_KEY);
    }
}

/// Token count for one context message, matching what actually gets serialized.
///
/// Reuses a stamped hint when one is present. The hint is only ever written
/// from a value this function produced, so reuse is exact rather than
/// approximate -- but a debug build cross-checks it anyway, because "exact by
/// construction" is a claim about code that changes.
pub fn count_message_tokens(msg: &Value) -> i32 {
    if token_reuse_enabled() {
        if let Some(hint) = msg.get(TOKEN_HINT_KEY).and_then(|v| v.as_i64()) {
            let hint = hint as i32;
            #[cfg(debug_assertions)]
            {
                let recomputed = count_message_tokens_uncached(msg);
                debug_assert_eq!(
                    hint, recomputed,
                    "a stamped token hint disagreed with a live recount; a \
                     transform mutated this message without clearing the hint"
                );
            }
            return hint;
        }
    }
    count_message_tokens_uncached(msg)
}

/// The real count, always computed.
fn count_message_tokens_uncached(msg: &Value) -> i32 {
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
                        } else if block_type == "image" || block_type == "image_url" {
                            // `image_url` is not a second spelling we accept
                            // for tidiness — it is the *only* shape that
                            // reaches a real request. `normalize_image_block`
                            // in `context/builder.rs` emits
                            // `{"type":"image_url", ...}`, so matching only
                            // "image" charged every attached image zero
                            // tokens in every budget: the wrap-up valve, the
                            // emergency trim, and the per-row `token_count`
                            // persisted for compaction all counted a
                            // multi-megabyte base64 payload as free.
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

    /// `normalize_image_block` (context/builder.rs) emits `"image_url"`, so
    /// an image charged nothing at all in every budget until that spelling
    /// was matched. Guard both the shape that ships and the shape the old
    /// arm handled.
    #[test]
    fn attached_images_are_never_free() {
        for block_type in ["image", "image_url"] {
            let msg = json!({
                "role": "user",
                "content": [{"type": block_type, "image_url": {"url": "data:image/png;base64,AAAA"}}]
            });
            let counted = count_message_tokens(&msg);
            assert!(
                counted >= 256,
                "{block_type:?} must be charged the flat image cost, got {counted}"
            );
        }
    }
}

/// How much of a delegate's run may go on reasoning.
///
/// Two shapes because the useful answer depends on what the caller knows. A
/// pipeline that has measured its own workload wants an absolute number; a
/// specialist definition that must work across a 8k local model and a 200k
/// hosted one wants a share of whatever room there is.
#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningCap {
    Tokens(i32),
    ContextFraction(f64),
}

/// Anthropic will not accept a `budget_tokens` below this, and treats a smaller
/// one as no thinking at all. A cap that cannot be expressed on the one dialect
/// with a real budget field would silently mean "off" there, so it is the floor
/// everywhere — a cap resolving below it is honestly reported as zero rather
/// than quietly rounded up.
pub const MIN_REASONING_BUDGET: i32 = 1024;

/// Fallback when the model's context window is unknown.
///
/// Deliberately a concrete number rather than "unbounded". An unknown window is
/// overwhelmingly the self-hosted case, which is also the case where nothing on
/// the wire enforces anything — so it is exactly where a missing cap does the
/// most damage, not the least.
pub const UNKNOWN_CONTEXT_REASONING_BUDGET: i32 = 16_384;

/// Resolve a cap to an absolute token budget for one run.
///
/// `report_reserve` is the output room the delegate still needs *after*
/// thinking, to write its structured answer. It is subtracted before the clamp
/// rather than after, because a budget that leaves no room for the report buys
/// a well-reasoned silence.
///
/// Returns 0 for "no reasoning at all", which is a real answer: on a small
/// window with a large schema there is genuinely nothing left to spend.
pub fn reasoning_budget_tokens(
    cap: ReasoningCap,
    context_length: Option<i32>,
    projected_input: i32,
    report_reserve: i32,
    ceiling: i32,
) -> i32 {
    let requested = match cap {
        ReasoningCap::Tokens(n) => n,
        // A fraction of what is *left*, not of the whole window: a delegate
        // sixty percent through its context has sixty percent less to spend on
        // thinking, which is the behaviour that keeps a long run from starving
        // its own conclusion.
        ReasoningCap::ContextFraction(f) => match context_length {
            Some(ctx) => {
                let remaining = ctx.saturating_sub(projected_input).max(0);
                (f64::from(remaining) * f.clamp(0.0, 1.0)) as i32
            }
            None => UNKNOWN_CONTEXT_REASONING_BUDGET,
        },
    };

    // The room that actually exists, once the report is accounted for.
    let headroom = match context_length {
        Some(ctx) => ctx
            .saturating_sub(projected_input)
            .saturating_sub(report_reserve.max(0))
            .max(0),
        None => i32::MAX,
    };

    let ceiling = ceiling.max(0).min(headroom);
    let budget = requested.max(0).min(ceiling);
    if budget < MIN_REASONING_BUDGET {
        0
    } else {
        budget
    }
}

#[cfg(test)]
mod reasoning_budget_tests {
    use super::*;

    #[test]
    fn an_explicit_token_cap_is_used_as_given() {
        assert_eq!(
            reasoning_budget_tokens(ReasoningCap::Tokens(8_000), Some(128_000), 1_000, 2_000, 32_768),
            8_000
        );
    }

    /// The fraction applies to remaining room, so the same cap yields less as
    /// the run fills its window.
    #[test]
    fn a_fraction_shrinks_as_the_context_fills() {
        let early = reasoning_budget_tokens(
            ReasoningCap::ContextFraction(0.25),
            Some(100_000),
            10_000,
            2_000,
            i32::MAX,
        );
        let late = reasoning_budget_tokens(
            ReasoningCap::ContextFraction(0.25),
            Some(100_000),
            80_000,
            2_000,
            i32::MAX,
        );
        assert_eq!(early, 22_500);
        assert_eq!(late, 5_000);
        assert!(late < early);
    }

    /// Unknown context is the self-hosted case, and the case where nothing on
    /// the wire enforces a cap — so it must not read as "unbounded".
    #[test]
    fn an_unknown_context_falls_back_to_a_conservative_absolute() {
        assert_eq!(
            reasoning_budget_tokens(
                ReasoningCap::ContextFraction(0.5),
                None,
                0,
                2_000,
                i32::MAX
            ),
            UNKNOWN_CONTEXT_REASONING_BUDGET
        );
    }

    /// The report has to fit. A budget that consumes the room the answer needs
    /// buys a well-reasoned silence.
    #[test]
    fn the_report_reserve_is_subtracted_before_the_clamp() {
        // 20k window, 6k already used, 3k held back for the report -> 11k left,
        // so the reserve is what bounds this, not the 50k the caller asked for.
        assert_eq!(
            reasoning_budget_tokens(
                ReasoningCap::Tokens(50_000),
                Some(20_000),
                6_000,
                3_000,
                i32::MAX,
            ),
            11_000
        );

        // A larger reserve takes room away from thinking, not from the answer.
        assert_eq!(
            reasoning_budget_tokens(
                ReasoningCap::Tokens(50_000),
                Some(20_000),
                6_000,
                9_000,
                i32::MAX,
            ),
            5_000
        );

        // And once the reserve leaves less than the floor, the honest answer is
        // no thinking at all rather than a budget Anthropic would reject.
        assert_eq!(
            reasoning_budget_tokens(
                ReasoningCap::Tokens(50_000),
                Some(10_000),
                6_000,
                3_000,
                i32::MAX,
            ),
            0,
            "1000 tokens of headroom is below MIN_REASONING_BUDGET"
        );
    }

    /// Below Anthropic's `budget_tokens` minimum, a cap means "no thinking",
    /// not "a tiny bit of thinking" — rounding up would send a value that
    /// dialect rejects.
    #[test]
    fn a_budget_under_the_floor_is_reported_as_none() {
        assert_eq!(
            reasoning_budget_tokens(ReasoningCap::Tokens(500), Some(128_000), 0, 0, i32::MAX),
            0
        );
        assert_eq!(
            reasoning_budget_tokens(
                ReasoningCap::Tokens(MIN_REASONING_BUDGET),
                Some(128_000),
                0,
                0,
                i32::MAX
            ),
            MIN_REASONING_BUDGET
        );
    }

    #[test]
    fn a_full_context_leaves_nothing_to_spend() {
        assert_eq!(
            reasoning_budget_tokens(
                ReasoningCap::ContextFraction(0.5),
                Some(8_000),
                8_000,
                2_000,
                i32::MAX
            ),
            0
        );
    }

    /// Nonsense input must not produce a nonsense budget: `anthropic.rs`
    /// silently discards an out-of-range value and falls back to a *larger*
    /// default, so a negative here would widen the budget rather than narrow it.
    #[test]
    fn negative_and_absurd_inputs_clamp_rather_than_invert() {
        assert_eq!(
            reasoning_budget_tokens(ReasoningCap::Tokens(-5), Some(128_000), 0, 0, i32::MAX),
            0
        );
        assert_eq!(
            reasoning_budget_tokens(
                ReasoningCap::ContextFraction(9.0),
                Some(100_000),
                0,
                0,
                32_768
            ),
            32_768,
            "a fraction above 1.0 is clamped, and the ceiling still binds"
        );
    }
}
