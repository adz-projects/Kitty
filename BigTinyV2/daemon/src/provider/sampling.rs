use super::base::SamplingParams;

/// Sampling floor applied when a self-hosted provider's profile leaves a
/// field unset. llama-server's own defaults disable repetition control
/// entirely (`repeat_penalty` 1.0, `presence_penalty`/`frequency_penalty`
/// 0.0), which is what let a quantized Qwen3.6 model stream the same clause
/// several hundred times with no way to escape. Qwen's own published
/// guidance for its instruct models is temperature 0.7 / top_p 0.8 / top_k
/// 20 / min_p 0, plus a non-zero presence penalty specifically to break
/// repetition loops on quantized weights — those numbers are also
/// unobjectionable defaults for other local instruct models, so they apply
/// to every self-hosted endpoint, not just Qwen.
///
/// `max_tokens` also gets a finite default here (independent of the loop
/// bug): no single reply should be able to stream forever, healthy or not.
/// [`DEFAULT_REPLY_CAP`] rather than a tighter figure — a long reply (a big
/// diff, a full file rewrite, a critique memo) hitting a low cap truncates
/// mid-output (the agent loop treats a `"length"` `finish_reason` as "not
/// done yet" and continues the reply in another step), and on a thinking
/// model the reasoning counts against the same cap: llama-server charges
/// `reasoning_content` tokens to `max_tokens`. 8192 was too tight for that —
/// a high-effort Qwen spent most of it thinking. The presence penalty, not
/// this cap, is what breaks repetition loops; the cap only bounds a runaway.
/// [`resolve`] lowers it to half the model's context window when that is
/// known, so a reply never claims room the prompt needs.
///
/// Only fills fields the profile left `None` — see `merge`. Hosted
/// providers (`anthropic`/`openai`/`openrouter`) get no defaults at all:
/// they tune their own sampling, and pushing `top_k`/`min_p` at them would
/// be a regression (those fields aren't even valid on their wire format).
/// The reply cap a self-hosted provider gets when its profile sets none.
pub const DEFAULT_REPLY_CAP: i32 = 32_768;

/// The smallest cap the context clamp leaves: below this a model with a tiny
/// window could not finish an ordinary answer.
const MIN_REPLY_CAP: i32 = 2_048;

pub fn defaults_for(provider_type: &str, _model: &str) -> SamplingParams {
    if matches!(provider_type, "ollama" | "custom_openai") {
        SamplingParams {
            temperature: Some(0.7),
            top_p: Some(0.8),
            top_k: Some(20),
            min_p: Some(0.0),
            presence_penalty: Some(1.0),
            frequency_penalty: None,
            max_tokens: Some(DEFAULT_REPLY_CAP),
            // Effort is a per-turn request set by the loop, never a provider
            // floor — a self-hosted endpoint has no reasoning-effort parameter
            // at all, so there is nothing to default here.
            effort: None,
            // Same reasoning as `effort`: a provider has no reasoning-budget
            // setting to default, only the agent loop knows the run's cap.
            reasoning_max_tokens: None,
        }
    } else {
        SamplingParams::default()
    }
}

/// Merge a provider's configured overrides on top of its model-aware
/// defaults — a configured field always wins; defaults only fill `None`.
pub fn merge(configured: &SamplingParams, defaults: &SamplingParams) -> SamplingParams {
    SamplingParams {
        temperature: configured.temperature.or(defaults.temperature),
        top_p: configured.top_p.or(defaults.top_p),
        top_k: configured.top_k.or(defaults.top_k),
        min_p: configured.min_p.or(defaults.min_p),
        presence_penalty: configured.presence_penalty.or(defaults.presence_penalty),
        frequency_penalty: configured.frequency_penalty.or(defaults.frequency_penalty),
        reasoning_max_tokens: configured
            .reasoning_max_tokens
            .or(defaults.reasoning_max_tokens),
        max_tokens: configured.max_tokens.or(defaults.max_tokens),
        // Neither presets nor floors ever set effort (the loop applies it after
        // this merge), so this `or` is only ever `None.or(None)` — carried
        // through for completeness rather than to combine two levels. `Effort`
        // is no longer `Copy` (it can carry a model-specific level string), so
        // clone out of the shared references.
        effort: configured.effort.clone().or_else(|| defaults.effort.clone()),
    }
}

/// Resolve the final `SamplingParams` for a provider registration in one
/// call: configured values from `ProviderConfig`, backfilled by
/// `defaults_for` when the provider is self-hosted. `context_length` is the
/// row's context window, when known: the *default* reply cap is lowered to
/// half of it (never below [`MIN_REPLY_CAP`]). A configured `max_tokens` is
/// the user's and is never clamped.
pub fn resolve(
    provider_type: &str,
    model: &str,
    configured: &SamplingParams,
    context_length: Option<i32>,
) -> SamplingParams {
    let mut defaults = defaults_for(provider_type, model);
    if let (Some(cap), Some(window)) = (defaults.max_tokens, context_length.filter(|&n| n > 0)) {
        defaults.max_tokens = Some(cap.min((window / 2).max(MIN_REPLY_CAP)));
    }
    merge(configured, &defaults)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_hosted_providers_get_the_qwen_recommended_floor() {
        let d = defaults_for("custom_openai", "qwen3.6");
        assert_eq!(d.temperature, Some(0.7));
        assert_eq!(d.top_p, Some(0.8));
        assert_eq!(d.top_k, Some(20));
        assert_eq!(d.min_p, Some(0.0));
        assert_eq!(d.presence_penalty, Some(1.0));
        assert_eq!(d.max_tokens, Some(DEFAULT_REPLY_CAP));

        assert_eq!(defaults_for("ollama", "llama3"), d);
    }

    #[test]
    fn hosted_providers_get_no_defaults_at_all() {
        for pt in ["anthropic", "openai", "openrouter"] {
            assert_eq!(defaults_for(pt, "anything"), SamplingParams::default());
        }
    }

    #[test]
    fn a_configured_field_always_wins_over_the_default() {
        let configured = SamplingParams {
            temperature: Some(0.1),
            ..Default::default()
        };
        let resolved = resolve("custom_openai", "qwen3.6", &configured, None);
        assert_eq!(resolved.temperature, Some(0.1));
        // Everything the profile left unset still gets the model-aware
        // default -- a user overriding just one knob shouldn't silently
        // lose repetition control on all the others.
        assert_eq!(resolved.presence_penalty, Some(1.0));
        assert_eq!(resolved.top_k, Some(20));
    }

    #[test]
    fn an_unconfigured_hosted_provider_sends_nothing() {
        let resolved = resolve("anthropic", "claude", &SamplingParams::default(), None);
        assert_eq!(resolved, SamplingParams::default());
        // A known window adds no cap where there was none.
        let resolved = resolve("anthropic", "claude", &SamplingParams::default(), Some(8_000));
        assert_eq!(resolved.max_tokens, None);
    }

    #[test]
    fn the_default_reply_cap_leaves_half_the_window_for_the_prompt() {
        let none = SamplingParams::default();
        let cap = |window| resolve("custom_openai", "qwen", &none, window).max_tokens;
        // A thinking model on a big window gets the whole default.
        assert_eq!(cap(Some(192_000)), Some(DEFAULT_REPLY_CAP));
        assert_eq!(cap(None), Some(DEFAULT_REPLY_CAP));
        // A small window halves it, down to a floor; nonsense windows are ignored.
        assert_eq!(cap(Some(16_384)), Some(8_192));
        assert_eq!(cap(Some(2_000)), Some(MIN_REPLY_CAP));
        assert_eq!(cap(Some(0)), Some(DEFAULT_REPLY_CAP));
        // The user's own cap is theirs: never raised or clamped.
        let set = SamplingParams {
            max_tokens: Some(100_000),
            ..Default::default()
        };
        assert_eq!(
            resolve("custom_openai", "qwen", &set, Some(16_384)).max_tokens,
            Some(100_000)
        );
    }
}
