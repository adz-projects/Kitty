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
/// 8192 rather than a tighter figure — a long reply (a big diff, a full
/// file rewrite) hitting a low cap truncates mid-output with no error (the
/// agent loop just treats a `"length"` `finish_reason` as "not done yet" and
/// burns another step to keep generating), so the floor needs enough
/// headroom that routine long replies don't trip it in the first place.
///
/// Only fills fields the profile left `None` — see `merge`. Hosted
/// providers (`anthropic`/`openai`/`openrouter`) get no defaults at all:
/// they tune their own sampling, and pushing `top_k`/`min_p` at them would
/// be a regression (those fields aren't even valid on their wire format).
pub fn defaults_for(provider_type: &str, _model: &str) -> SamplingParams {
    if matches!(provider_type, "ollama" | "custom_openai") {
        SamplingParams {
            temperature: Some(0.7),
            top_p: Some(0.8),
            top_k: Some(20),
            min_p: Some(0.0),
            presence_penalty: Some(1.0),
            frequency_penalty: None,
            max_tokens: Some(8192),
            // Effort is a per-turn request set by the loop, never a provider
            // floor — a self-hosted endpoint has no reasoning-effort parameter
            // at all, so there is nothing to default here.
            effort: None,
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
        max_tokens: configured.max_tokens.or(defaults.max_tokens),
        // Neither presets nor floors ever set effort (the loop applies it after
        // this merge), so this `or` is only ever `None.or(None)` — carried
        // through for completeness rather than to combine two levels.
        effort: configured.effort.or(defaults.effort),
    }
}

/// Resolve the final `SamplingParams` for a provider registration in one
/// call: configured values from `ProviderConfig`, backfilled by
/// `defaults_for` when the provider is self-hosted.
pub fn resolve(provider_type: &str, model: &str, configured: &SamplingParams) -> SamplingParams {
    merge(configured, &defaults_for(provider_type, model))
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
        assert_eq!(d.max_tokens, Some(8192));

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
        let resolved = resolve("custom_openai", "qwen3.6", &configured);
        assert_eq!(resolved.temperature, Some(0.1));
        // Everything the profile left unset still gets the model-aware
        // default -- a user overriding just one knob shouldn't silently
        // lose repetition control on all the others.
        assert_eq!(resolved.presence_penalty, Some(1.0));
        assert_eq!(resolved.top_k, Some(20));
    }

    #[test]
    fn an_unconfigured_hosted_provider_sends_nothing() {
        let resolved = resolve("anthropic", "claude", &SamplingParams::default());
        assert_eq!(resolved, SamplingParams::default());
    }
}
