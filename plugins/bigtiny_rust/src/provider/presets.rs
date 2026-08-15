//! Sampling presets (docs/ANDROID.md §6.2, D6).
//!
//! A session picks one by name; `agent::loop_` merges it over the provider's
//! own resolved sampling, so a preset is an *override* rather than a
//! replacement — anything a preset leaves unset still falls through to
//! `provider::sampling`'s per-dialect floor.
//!
//! **Every field is `Some`, deliberately.** `sampling::merge` is
//! `configured.or(defaults)`, so a `None` doesn't clear a value, it inherits
//! one — and the self-hosted floor sets `presence_penalty: Some(1.0)`. A
//! "Creative" preset that left `presence_penalty` unset would silently keep
//! the floor's repetition penalty and not be creative in the way its name
//! promises. Spelling each field out is what makes a preset mean what it says.

use super::base::SamplingParams;

/// Resolve a preset name to its params. `None` for an unknown name (including
/// the empty string), which callers treat as "no preset" — an unrecognised
/// value must not silently apply someone else's settings.
pub fn resolve(name: &str) -> Option<SamplingParams> {
    match name.trim().to_ascii_lowercase().as_str() {
        "precise" => Some(SamplingParams {
            temperature: Some(0.1),
            top_k: Some(50),
            top_p: Some(0.95),
            min_p: Some(0.0),
            // §6.2 lists `repeat_penalty`, which llama.cpp exposes but the
            // OpenAI-compatible wire format doesn't. `presence_penalty` is
            // the field that actually reaches every provider, so the table's
            // 1.05/1.05/1.1 maps onto it rather than being dropped.
            presence_penalty: Some(0.05),
            frequency_penalty: Some(0.0),
            max_tokens: None,
            // A preset is about creativity, not reasoning — effort is set by
            // the loop from the session's own thinking-effort choice.
            effort: None,
        }),
        "balanced" => Some(SamplingParams {
            temperature: Some(0.6),
            top_k: Some(40),
            top_p: Some(0.9),
            min_p: Some(0.0),
            presence_penalty: Some(0.05),
            frequency_penalty: Some(0.0),
            max_tokens: None,
            // A preset is about creativity, not reasoning — effort is set by
            // the loop from the session's own thinking-effort choice.
            effort: None,
        }),
        "creative" => Some(SamplingParams {
            temperature: Some(1.0),
            // §6.2's "0 (off)" — `build_sampler` skips `top_k` at 0, which is
            // what "off" means to llama.cpp.
            top_k: Some(0),
            top_p: Some(1.0),
            min_p: Some(0.0),
            presence_penalty: Some(0.1),
            frequency_penalty: Some(0.0),
            max_tokens: None,
            // A preset is about creativity, not reasoning — effort is set by
            // the loop from the session's own thinking-effort choice.
            effort: None,
        }),
        _ => None,
    }
}

/// The preset names the UI offers, in the order §6.2 lists them.
pub const NAMES: [&str; 3] = ["precise", "balanced", "creative"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_documented_presets_resolve() {
        for name in NAMES {
            assert!(resolve(name).is_some(), "{name} should resolve");
        }
        assert_eq!(resolve("Precise").unwrap().temperature, Some(0.1));
        assert_eq!(resolve(" BALANCED ").unwrap().temperature, Some(0.6));
        assert_eq!(resolve("creative").unwrap().temperature, Some(1.0));
    }

    #[test]
    fn an_unknown_or_empty_preset_resolves_to_nothing() {
        assert!(resolve("").is_none());
        assert!(resolve("turbo").is_none());
    }

    /// The reason every field is `Some`: `merge` is `configured.or(defaults)`,
    /// so a `None` inherits the self-hosted floor's `presence_penalty: 1.0`
    /// rather than clearing it. A preset that didn't set it would quietly not
    /// mean what its name says.
    #[test]
    fn every_preset_pins_the_fields_the_self_hosted_floor_would_otherwise_supply() {
        let floor = super::super::sampling::defaults_for("custom_openai", "");
        assert!(
            floor.presence_penalty.is_some(),
            "this test is only meaningful while the floor sets a presence penalty"
        );
        for name in NAMES {
            let p = resolve(name).unwrap();
            assert!(p.temperature.is_some(), "{name}: temperature");
            assert!(p.top_k.is_some(), "{name}: top_k");
            assert!(p.top_p.is_some(), "{name}: top_p");
            assert!(p.min_p.is_some(), "{name}: min_p");
            assert!(p.presence_penalty.is_some(), "{name}: presence_penalty");
        }
    }

    /// Merged over the floor, a preset's values must actually win — this is
    /// the property the whole seam depends on.
    #[test]
    fn a_preset_overrides_the_provider_floor() {
        let floor = super::super::sampling::defaults_for("custom_openai", "");
        let merged = super::super::sampling::merge(&resolve("creative").unwrap(), &floor);
        assert_eq!(merged.temperature, Some(1.0));
        assert_eq!(merged.presence_penalty, Some(0.1));
        assert_ne!(
            merged.presence_penalty, floor.presence_penalty,
            "the preset must displace the floor, not inherit it"
        );
    }

    /// `max_tokens` is deliberately left unset: it's a budget, not a style
    /// choice, and the provider's own value (or the caller's) should win.
    #[test]
    fn presets_do_not_cap_max_tokens() {
        let floor = super::super::sampling::defaults_for("custom_openai", "");
        let merged = super::super::sampling::merge(&resolve("precise").unwrap(), &floor);
        assert_eq!(merged.max_tokens, floor.max_tokens);
    }
}
