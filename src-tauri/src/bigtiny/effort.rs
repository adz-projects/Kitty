//! Which providers/models actually accept a reasoning-effort control, and the
//! option set to offer for each — the "shown only where supported" half of the
//! thinking-effort feature.
//!
//! This lives in Kitty, not the daemon, on purpose: the daemon's provider row
//! is CHECK-constrained to `('openai_compat', 'anthropic')` and can't tell
//! OpenAI from OpenRouter, but Kitty's `ProviderProfile::provider_type` keeps
//! the finer distinction (`openai`/`openrouter`/`ollama`/`custom_openai`/
//! `local`) that decides the dialect. It deliberately does *not* reuse
//! `src/lib/reasoning_models.ts`, which answers a different question ("does this
//! model stream a visible trace"): the sets genuinely diverge — `deepseek-r1`
//! streams a trace but takes no effort parameter, and a current Claude takes a
//! thinking budget but matches none of that file's patterns.
//!
//! Model matching is a *denylist* for the vendors that ship ids constantly
//! (Anthropic, OpenAI) and a small allowlist for OpenRouter's grab-bag — a
//! denylist of retired families ages better than an allowlist chased against a
//! moving catalog.

use crate::commands::{EffortOption, ThinkingEffort};
use tauri::{AppHandle, Manager};

/// How a provider expresses reasoning effort on the wire. Kitty only needs to
/// know *whether* a control exists (to show the dropdown) and which option set
/// fits; the actual wire translation happens daemon-side per dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortDialect {
    /// OpenAI o-series / gpt-5: a flat `reasoning_effort` string, and no way to
    /// turn reasoning *off* — so its options start at Low.
    OpenAiReasoningEffort,
    /// OpenRouter: a nested `reasoning` object that *can* be disabled.
    OpenRouterReasoning,
    /// Anthropic: an extended-thinking token budget, also disable-able.
    AnthropicThinking,
    /// A self-hosted OpenAI-compatible server (llama.cpp `llama-server`,
    /// Ollama) running a reasoning-capable model. These honor neither OpenAI's
    /// `reasoning_effort` nor OpenRouter's `reasoning` object; the portable
    /// control is the chat template's `enable_thinking` kwarg (Qwen3 et al.),
    /// so it's a simple on/off toggle rather than graded levels.
    LlamaServerThinking,
}

/// The effort control for a `(provider_type, model)` pair, or `None` when the
/// model has no such knob — which is what hides the dropdown.
pub fn effort_dialect(provider_type: &str, model: &str) -> Option<EffortDialect> {
    match provider_type {
        "anthropic" if anthropic_takes_effort(model) => Some(EffortDialect::AnthropicThinking),
        "openai" if openai_takes_effort(model) => Some(EffortDialect::OpenAiReasoningEffort),
        "openrouter" if openrouter_takes_effort(model) => {
            Some(EffortDialect::OpenRouterReasoning)
        }
        // A self-hosted OpenAI-compatible endpoint (a llama-server or Ollama
        // the user runs) with a reasoning-capable model gets an on/off thinking
        // toggle wired to the chat template's `enable_thinking` kwarg. A plain
        // chat model on the same endpoint still gets nothing.
        "custom_openai" | "ollama" if self_hosted_takes_thinking(model) => {
            Some(EffortDialect::LlamaServerThinking)
        }
        // `local` (the in-process engine) has no such control, and any
        // self-hosted endpoint serving a non-reasoning model falls here too —
        // the case the whole "hidden otherwise" behavior exists for.
        _ => None,
    }
}

/// The options to offer for a dialect. OpenAI can't disable reasoning, so it
/// omits "Off"; the others lead with it.
pub fn effort_options(dialect: EffortDialect) -> Vec<EffortOption> {
    let opt = |name: &str, value: &str| EffortOption {
        name: name.to_string(),
        value: value.to_string(),
    };
    match dialect {
        EffortDialect::OpenAiReasoningEffort => {
            vec![opt("Low", "low"), opt("Medium", "medium"), opt("High", "high")]
        }
        EffortDialect::OpenRouterReasoning | EffortDialect::AnthropicThinking => vec![
            opt("Off", "off"),
            opt("Low", "low"),
            opt("Medium", "medium"),
            opt("High", "high"),
        ],
        // On/Off only — a chat-template toggle, not graded levels. "On" rides
        // the wire value "high" so the daemon's existing `Effort::from_wire`
        // maps it to a positive level (→ `enable_thinking: true`); "off" maps
        // to `Effort::Off` (→ `enable_thinking: false`).
        EffortDialect::LlamaServerThinking => {
            vec![opt("Thinking off", "off"), opt("Thinking on", "high")]
        }
    }
}

/// The default effort when a session has never chosen one. OpenAI's o-series
/// reasons no matter what, so "Medium" is the honest resting state; the others
/// default to "Off" (opt-in).
fn default_value(dialect: EffortDialect) -> &'static str {
    match dialect {
        EffortDialect::OpenAiReasoningEffort => "medium",
        EffortDialect::OpenRouterReasoning | EffortDialect::AnthropicThinking => "off",
        // Matches the model's own resting behavior — Qwen3 and friends think by
        // default — so the displayed state agrees with what an untouched
        // session actually does (no `thinking_effort` sent → server default).
        EffortDialect::LlamaServerThinking => "high",
    }
}

/// The `ThinkingEffort` payload for a session, derived from the **active**
/// provider profile's type+model — `None` when that provider has no effort
/// control (the dropdown then stays hidden). The current value is the
/// session's persisted choice if it's still one of the dialect's options,
/// otherwise the dialect default (a value carried over from a different
/// provider must not stick when it isn't offered here).
pub fn thinking_effort_for(app: &AppHandle, session_id: &str) -> Option<ThinkingEffort> {
    let state = app.state::<crate::state::AppState>();
    let cfg = state.config.lock().unwrap();
    let active_id = cfg.active_provider_id.as_deref()?;
    let profile = cfg.providers.iter().find(|p| p.id == active_id)?;
    let model = profile.models.first().map(String::as_str).unwrap_or("");
    let dialect = effort_dialect(&profile.provider_type, model)?;
    let options = effort_options(dialect);
    let current_value = cfg
        .session_efforts
        .get(session_id)
        .filter(|v| options.iter().any(|o| &o.value == *v))
        .cloned()
        .unwrap_or_else(|| default_value(dialect).to_string());
    Some(ThinkingEffort {
        current_value,
        options,
    })
}

/// `claude-*` minus the families that predate extended thinking. A denylist so
/// a newly-shipped Claude id is assumed capable rather than needing this file
/// edited before the dropdown appears.
fn anthropic_takes_effort(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    if !m.contains("claude") {
        return false;
    }
    // Retired / non-thinking families.
    const DENY: [&str; 6] = [
        "claude-3-5",
        "claude-3.5",
        "claude-3-opus",
        "claude-3-haiku",
        "claude-3-sonnet",
        "claude-2",
    ];
    !DENY.iter().any(|d| m.contains(d))
}

/// OpenAI's reasoning families: the o-series (`o1`..`o4`, alone or with a
/// suffix) and `gpt-5*`.
fn openai_takes_effort(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    // Strip any `vendor/` prefix an OpenRouter-style id might carry, so this
    // is reusable from `openrouter_takes_effort`.
    let bare = m.rsplit('/').next().unwrap_or(&m);
    if bare.starts_with("gpt-5") {
        return true;
    }
    for n in ["o1", "o2", "o3", "o4"] {
        if bare == n || bare.starts_with(&format!("{n}-")) {
            return true;
        }
    }
    false
}

/// OpenRouter carries everything, so its capability is the union of the OpenAI
/// and Anthropic reasoning families plus a small allowlist of other reasoning
/// models it hosts. If OpenRouter turns out to 400 on a `reasoning` block for
/// an unsupported model, tighten this to a strict allowlist.
fn openrouter_takes_effort(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    let bare = m.rsplit('/').next().unwrap_or(&m);
    if openai_takes_effort(bare) || anthropic_takes_effort(bare) {
        return true;
    }
    bare.contains("deepseek-r1")
        || (bare.contains("qwen3") && bare.contains("thinking"))
        || (bare.contains("grok") && (bare.contains("mini") || bare.contains("reasoning")))
        || bare.contains("gemini-2.5")
        || bare.contains("glm-4")
}

/// Reasoning-capable model families that a self-hosted OpenAI-compatible server
/// commonly runs. Unlike `openrouter_takes_effort`, this does **not** require a
/// `thinking` suffix — a self-hosted Qwen3 id is usually just `qwen3-30b` /
/// `Qwen3.8-27b`, and thinking is a template toggle, not a separate model.
/// Extend freely: a false negative just hides the toggle for that model.
fn self_hosted_takes_thinking(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    let bare = m.rsplit('/').next().unwrap_or(&m);
    bare.contains("qwen3")
        || bare.contains("qwq")
        || bare.contains("deepseek-r1")
        || bare.contains("glm-4")
        || bare.contains("magistral")
        || bare.contains("gpt-oss")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_o_series_and_gpt5_take_effort() {
        assert!(effort_dialect("openai", "o1").is_some());
        assert!(effort_dialect("openai", "o3-mini").is_some());
        assert!(effort_dialect("openai", "o4-mini-2025").is_some());
        assert!(effort_dialect("openai", "gpt-5").is_some());
        assert!(effort_dialect("openai", "gpt-5-mini").is_some());
        // Non-reasoning OpenAI models get nothing.
        assert!(effort_dialect("openai", "gpt-4o").is_none());
        assert!(effort_dialect("openai", "gpt-4.1").is_none());
        // `o` must be a real o-series id, not any word starting with o.
        assert!(effort_dialect("openai", "omni-1").is_none());
    }

    #[test]
    fn anthropic_denylists_pre_thinking_families() {
        assert!(effort_dialect("anthropic", "claude-sonnet-4-20250514").is_some());
        assert!(effort_dialect("anthropic", "claude-opus-4-8").is_some());
        assert!(effort_dialect("anthropic", "claude-3-7-sonnet").is_some());
        // Retired / non-thinking families.
        assert!(effort_dialect("anthropic", "claude-3-5-sonnet-20241022").is_none());
        assert!(effort_dialect("anthropic", "claude-3-opus-20240229").is_none());
        assert!(effort_dialect("anthropic", "claude-3-haiku-20240307").is_none());
        // The original claude-3-sonnet predates extended thinking too — a
        // `thinking` block gets a 400 from Anthropic (815bugs #11).
        assert!(effort_dialect("anthropic", "claude-3-sonnet-20240229").is_none());
        assert!(effort_dialect("anthropic", "claude-2.1").is_none());
        // Not a Claude at all.
        assert!(effort_dialect("anthropic", "some-other-model").is_none());
    }

    #[test]
    fn openrouter_is_the_union_plus_extras() {
        assert!(effort_dialect("openrouter", "openai/o3").is_some());
        assert!(effort_dialect("openrouter", "anthropic/claude-sonnet-4").is_some());
        assert!(effort_dialect("openrouter", "deepseek/deepseek-r1").is_some());
        assert!(effort_dialect("openrouter", "qwen/qwen3-235b-thinking").is_some());
        assert!(effort_dialect("openrouter", "x-ai/grok-3-mini").is_some());
        assert!(effort_dialect("openrouter", "google/gemini-2.5-pro").is_some());
        assert!(effort_dialect("openrouter", "z-ai/glm-4.6").is_some());
        // A plain chat model on OpenRouter still gets nothing.
        assert!(effort_dialect("openrouter", "meta-llama/llama-3.3-70b").is_none());
        assert!(effort_dialect("openrouter", "anthropic/claude-3-5-haiku").is_none());
    }

    #[test]
    fn self_hosted_reasoning_models_get_an_on_off_thinking_toggle() {
        for pt in ["ollama", "custom_openai"] {
            // The user's exact model, plus other common self-hosted reasoners.
            assert_eq!(
                effort_dialect(pt, "Qwen3.8-27b"),
                Some(EffortDialect::LlamaServerThinking)
            );
            assert_eq!(
                effort_dialect(pt, "qwen3-30b"),
                Some(EffortDialect::LlamaServerThinking)
            );
            assert_eq!(
                effort_dialect(pt, "deepseek-r1-distill-qwen-7b"),
                Some(EffortDialect::LlamaServerThinking)
            );
            // A plain chat model on the same endpoint still gets nothing.
            assert!(effort_dialect(pt, "llama-3.3-70b-instruct").is_none());
        }
    }

    #[test]
    fn local_engine_never_takes_effort_even_for_a_reasoning_model() {
        assert!(effort_dialect("local", "qwen3-30b").is_none());
        assert!(effort_dialect("local", "o3").is_none());
    }

    #[test]
    fn openai_options_omit_off_others_lead_with_it() {
        let oai = effort_options(EffortDialect::OpenAiReasoningEffort);
        assert!(!oai.iter().any(|o| o.value == "off"));
        assert_eq!(oai.first().unwrap().value, "low");

        let anth = effort_options(EffortDialect::AnthropicThinking);
        assert_eq!(anth.first().unwrap().value, "off");
    }

    #[test]
    fn self_hosted_toggle_is_off_plus_on_as_high() {
        let opts = effort_options(EffortDialect::LlamaServerThinking);
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].value, "off");
        // "On" rides "high" so the daemon's Effort::from_wire maps it to a
        // positive level → enable_thinking: true.
        assert_eq!(opts[1].value, "high");
        assert_eq!(default_value(EffortDialect::LlamaServerThinking), "high");
    }
}
