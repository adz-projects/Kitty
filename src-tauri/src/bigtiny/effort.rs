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
    /// Ollama) running a reasoning-capable model. The daemon sends both the
    /// chat template's `enable_thinking` kwarg (Qwen3 et al., a boolean toggle)
    /// and a `reasoning_effort` string, so graded Off/Low/Medium/High levels are
    /// meaningful on a build that honors effort (gpt-oss, recent llama-server)
    /// and collapse to on/off on one that only has the template toggle.
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
        // the user runs). Unlike the hosted dialects, we don't guess from the
        // model *name* whether it reasons — we ask the endpoint (and, failing
        // that, HuggingFace) for the model's own effort levels and hide the
        // control when neither yields any (see `ensure_effort_levels_cached` /
        // `thinking_effort_for`). So every self-hosted endpoint takes this
        // dialect; discovery decides whether the dropdown actually shows.
        "custom_openai" | "ollama" => Some(EffortDialect::LlamaServerThinking),
        // `local` (the in-process engine) has no such control.
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
        // Self-hosted gets the same graded set as OpenRouter/Anthropic: the
        // daemon translates each level into `enable_thinking` (on for any
        // non-Off level) plus a `reasoning_effort` string, so Low/Medium/High
        // are distinct on servers that honor effort and all read as "thinking
        // on" on ones that only have the boolean template toggle.
        EffortDialect::OpenRouterReasoning
        | EffortDialect::AnthropicThinking
        | EffortDialect::LlamaServerThinking => vec![
            opt("Off", "off"),
            opt("Low", "low"),
            opt("Medium", "medium"),
            opt("High", "high"),
        ],
    }
}

/// The default effort when neither the session nor its model has a remembered
/// choice: **Medium**, uniformly, for every dialect that has a control at all.
///
/// It used to vary per dialect — "off" for OpenRouter/Anthropic (reasoning as
/// opt-in) and "high" for self-hosted (matching the model's own resting
/// behaviour). Both are defensible in isolation and neither survives contact
/// with switching between them: the same new chat reasoned hard, not at all, or
/// somewhere in between depending purely on which provider happened to be
/// active. Medium is the one setting that means the same thing everywhere, and
/// `confirm_model_effort` now pushes it to the daemon on the first turn, so the
/// displayed default is also what the session actually runs at rather than
/// whatever the server would have picked unasked.
fn default_value(_dialect: EffortDialect) -> &'static str {
    "medium"
}

/// The preferred default among a *discovered* level set (the self-hosted
/// dialect, whose levels come from the model's own chat template). Medium when
/// the model offers it; otherwise the template's own default, which is its
/// first — highest — level.
fn preferred_default(options: &[EffortOption]) -> String {
    options
        .iter()
        .find(|o| o.value == "medium")
        .or_else(|| options.first())
        .map(|o| o.value.clone())
        .unwrap_or_default()
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

    // The self-hosted dialect's levels are the *model's own* — discovered from
    // its chat template (see `probe_effort_levels`) and cached per provider.
    // "Only the model's levels" (no synthetic Off); empty/unprobed → hide.
    let (options, default) = match dialect {
        EffortDialect::LlamaServerThinking => {
            let cache = state.effort_levels.lock().unwrap();
            let levels = cache.get(&effort_cache_key(active_id, model))?;
            if levels.is_empty() {
                return None;
            }
            let opts: Vec<EffortOption> = levels
                .iter()
                .map(|l| EffortOption {
                    name: pretty_level(l),
                    value: l.clone(),
                })
                .collect();
            let default = preferred_default(&opts);
            (opts, default)
        }
        _ => (effort_options(dialect), default_value(dialect).to_string()),
    };

    // Resolution order: this session's own choice, then whatever was last
    // confirmed for this exact provider+model, then the dialect default. Each
    // candidate still has to be one of the offered options — a value carried
    // over from a different provider (or from a template whose levels have
    // since changed) must not stick when it isn't on the menu here.
    let offered = |v: &String| options.iter().any(|o| &o.value == v);
    let current_value = cfg
        .session_efforts
        .get(session_id)
        .filter(|v| offered(v))
        .or_else(|| {
            cfg.model_efforts
                .get(&effort_cache_key(active_id, model))
                .filter(|v| offered(v))
        })
        .cloned()
        .unwrap_or(default);
    Some(ThinkingEffort {
        current_value,
        options,
    })
}

/// A human label for a raw effort level. Known levels get a friendly name; an
/// unrecognized one is title-cased so a novel model level still reads sanely.
fn pretty_level(level: &str) -> String {
    match level {
        "xhigh" => "Extra high".to_string(),
        "high" => "High".to_string(),
        "medium" => "Medium".to_string(),
        "low" => "Low".to_string(),
        "minimal" => "Minimal".to_string(),
        other => {
            let mut c = other.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => other.to_string(),
            }
        }
    }
}

/// Pull the ordered reasoning-effort levels out of a chat template that guards
/// them, e.g. Qwen3's
/// `{%- if resolved_reasoning_effort not in ('xhigh', 'medium', 'low') %}`.
/// Returns the levels in template order (highest first), or `None` if the
/// template has no such guard (i.e. the model has no graded effort control).
///
/// Deliberately hand-parsed rather than regex to avoid pulling a regex
/// dependency into this crate for one pattern.
fn extract_effort_levels(template: &str) -> Option<Vec<String>> {
    let anchor = template.find("reasoning_effort")?;
    let after = &template[anchor..];
    let not_in = after.find("not in")?;
    let rest = &after[not_in..];
    let open = rest.find('(')?;
    let close = rest[open..].find(')')? + open;
    let inside = &rest[open + 1..close];
    // Extract single- or double-quoted tokens in order.
    let mut levels = Vec::new();
    let mut chars = inside.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch == '\'' || ch == '"' {
            let quote = ch;
            let mut token = String::new();
            for (_, c) in chars.by_ref() {
                if c == quote {
                    break;
                }
                token.push(c);
            }
            let token = token.trim().to_ascii_lowercase();
            if !token.is_empty() && !levels.contains(&token) {
                levels.push(token);
            }
        }
    }
    if levels.is_empty() {
        None
    } else {
        Some(levels)
    }
}

/// Cache key for discovered effort levels: keyed by provider **and** model, so
/// switching the model on a provider re-probes rather than showing another
/// model's levels.
fn effort_cache_key(provider_id: &str, model: &str) -> String {
    format!("{provider_id}\u{0}{model}")
}

/// Normalize a provider base URL to an origin we can hang `/props` off: add a
/// scheme if the user typed a bare `host:port`, and drop a trailing slash.
fn normalize_base(base_url: &str) -> String {
    let b = base_url.trim().trim_end_matches('/');
    if b.contains("://") {
        b.to_string()
    } else {
        format!("http://{b}")
    }
}

/// Discover a self-hosted model's reasoning-effort levels, in priority order:
///   1. **the endpoint** — `GET {base}/props`, read `chat_template`, extract the
///      levels its guard names (the real source: it's what the server enforces);
///   2. **HuggingFace** — when `model` is an `owner/repo`, its
///      `tokenizer_config.json` `chat_template`, same extraction;
///   3. otherwise `None`, which hides the dropdown.
/// Best-effort throughout: any network/parse failure falls through to the next
/// source, then to `None`.
async fn probe_effort_levels(
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
) -> Option<Vec<String>> {
    let client = crate::util::http_client();

    // 1. The endpoint's own chat template.
    let props_url = format!("{}/props", normalize_base(base_url));
    let mut req = client.get(&props_url);
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        req = req.bearer_auth(key);
    }
    if let Ok(resp) = req.send().await {
        if let Ok(v) = resp.json::<serde_json::Value>().await {
            if let Some(tmpl) = v.get("chat_template").and_then(|t| t.as_str()) {
                if let Some(levels) = extract_effort_levels(tmpl) {
                    tracing::debug!(?levels, "effort levels from endpoint /props");
                    return Some(levels);
                }
            }
        }
    }

    // 2. HuggingFace, only when the model id is a plausible `owner/repo` (not a
    //    local gguf path or a bare name).
    if model.split('/').count() == 2 && !model.contains(' ') && !model.starts_with('/') {
        let hf_url = format!(
            "https://huggingface.co/{model}/resolve/main/tokenizer_config.json"
        );
        if let Ok(resp) = client.get(&hf_url).send().await {
            if let Ok(v) = resp.json::<serde_json::Value>().await {
                if let Some(tmpl) = v.get("chat_template").and_then(|t| t.as_str()) {
                    if let Some(levels) = extract_effort_levels(tmpl) {
                        tracing::debug!(?levels, "effort levels from HuggingFace");
                        return Some(levels);
                    }
                }
            }
        }
    }

    None
}

/// Populate `AppState::effort_levels` for the active provider if it's a
/// self-hosted reasoning endpoint and hasn't been probed for this
/// provider+model yet. Called from the effort commands before
/// [`thinking_effort_for`] reads the cache. Cheap after the first probe (the
/// result, including "none found", is memoized).
pub async fn ensure_effort_levels_cached(app: &AppHandle) {
    let (key, base_url, model, provider_id) = {
        let state = app.state::<crate::state::AppState>();
        let cfg = state.config.lock().unwrap();
        let Some(active_id) = cfg.active_provider_id.clone() else {
            return;
        };
        let Some(profile) = cfg.providers.iter().find(|p| p.id == active_id) else {
            return;
        };
        let model = profile.models.first().cloned().unwrap_or_default();
        if !matches!(
            effort_dialect(&profile.provider_type, &model),
            Some(EffortDialect::LlamaServerThinking)
        ) {
            return;
        }
        let key = effort_cache_key(&active_id, &model);
        if state.effort_levels.lock().unwrap().contains_key(&key) {
            return; // already probed
        }
        (key, profile.base_url.clone(), model, active_id)
    };

    let api_key = crate::config::providers::get_secret_async(&provider_id).await;
    // Empty vec is a real answer ("probed, none found" → hide) and is cached so
    // we don't re-probe a server that has no graded effort every dropdown read.
    let levels = probe_effort_levels(&base_url, &model, api_key.as_deref())
        .await
        .unwrap_or_default();
    let state = app.state::<crate::state::AppState>();
    state.effort_levels.lock().unwrap().insert(key, levels);
}

/// Materialize a chat's effective reasoning effort on its **first turn**:
/// persist it as the session's own choice, remember it as this model's default
/// for next time, and push it to the daemon so the wire matches the dropdown.
///
/// Until this runs, a session that has never touched the dropdown shows a
/// default it never actually sent — `set_thinking_effort` is the only thing
/// that PATCHes the daemon, so an untouched session ran at whatever the
/// provider's own resting behaviour was while the UI claimed otherwise. Doing
/// it at first send (rather than at session creation) is what lets the user
/// pick a level in the header before sending and have *that* be the value
/// confirmed and remembered.
///
/// Every write is conditional on an actual change, so a chat whose effort
/// already matches its model's remembered value touches neither the config file
/// nor the daemon. Failures are logged, never surfaced: this is bookkeeping
/// around the turn, and the turn must still send if it fails.
pub async fn confirm_model_effort(app: &AppHandle, session_id: &str) {
    // Self-hosted levels have to be discovered before the resolved value means
    // anything — without this the first turn after a restart would confirm
    // against an empty option set.
    ensure_effort_levels_cached(app).await;
    let Some(effort) = thinking_effort_for(app, session_id) else {
        return; // no effort control on this provider — nothing to confirm
    };
    let value = effort.current_value;

    let push_to_daemon = {
        let state = app.state::<crate::state::AppState>();
        let mut cfg = state.config.lock().unwrap();
        let key = match cfg.active_provider_id.as_deref().and_then(|id| {
            let model = cfg
                .providers
                .iter()
                .find(|p| p.id == id)?
                .models
                .first()
                .map(String::as_str)
                .unwrap_or("");
            Some(effort_cache_key(id, model))
        }) {
            Some(k) => k,
            None => return,
        };

        let session_changed = cfg.session_efforts.get(session_id) != Some(&value);
        let model_changed = cfg.model_efforts.get(&key) != Some(&value);
        if session_changed {
            cfg.session_efforts
                .insert(session_id.to_string(), value.clone());
        }
        if model_changed {
            cfg.model_efforts.insert(key, value.clone());
        }
        if session_changed || model_changed {
            if let Err(e) = crate::config::save(&cfg) {
                tracing::warn!("failed to persist confirmed thinking effort: {e}");
            }
        }
        // Only a session not already carrying this value needs the daemon told;
        // one that does was PATCHed when it was set.
        session_changed
    };

    if push_to_daemon {
        if let Err(e) =
            crate::bigtiny::sessions::update_thinking_effort(app, session_id, &value).await
        {
            tracing::warn!("failed to push confirmed thinking effort to the daemon: {e}");
        }
    }
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
    fn every_self_hosted_endpoint_takes_the_dialect_visibility_is_discovery_driven() {
        // The dialect no longer guesses from the model *name* — every
        // self-hosted endpoint takes it, and runtime discovery
        // (`extract_effort_levels` / the cache) decides whether the dropdown
        // shows. So even a plain chat model resolves to the dialect here; it
        // just yields no levels at probe time and is hidden then.
        for pt in ["ollama", "custom_openai"] {
            for model in ["Qwen3.8-27b", "qwen3-30b", "llama-3.3-70b-instruct", ""] {
                assert_eq!(
                    effort_dialect(pt, model),
                    Some(EffortDialect::LlamaServerThinking),
                    "{pt}/{model}"
                );
            }
        }
    }

    /// The real Qwen3.8-27B guard, verbatim from the user's llama-server
    /// `/props` chat template — levels come back in template order, highest
    /// first, and deduplicated/lowercased.
    #[test]
    fn extract_levels_from_the_qwen3_template_guard() {
        let tmpl = "{%- set x = reasoning_effort|default('xhigh') %}\n\
                    {%- if resolved_reasoning_effort not in ('xhigh', 'medium', 'low') %}\n\
                    {{- raise_exception('...') }}";
        assert_eq!(
            extract_effort_levels(tmpl),
            Some(vec!["xhigh".into(), "medium".into(), "low".into()])
        );
    }

    #[test]
    fn a_template_with_no_effort_guard_yields_none() {
        assert_eq!(extract_effort_levels("{{ messages[0].content }}"), None);
    }

    #[test]
    fn every_dialect_defaults_to_medium() {
        // The owner ask: a new chat starts at Medium wherever there is a
        // control at all, instead of "off" on the hosted dialects and "high"
        // on a self-hosted one.
        for d in [
            EffortDialect::OpenAiReasoningEffort,
            EffortDialect::OpenRouterReasoning,
            EffortDialect::AnthropicThinking,
            EffortDialect::LlamaServerThinking,
        ] {
            assert_eq!(default_value(d), "medium", "{d:?}");
            // ...and Medium is always actually on the menu, so the default is
            // never a value the dropdown would reject.
            assert!(
                effort_options(d).iter().any(|o| o.value == "medium"),
                "{d:?} offers no medium option"
            );
        }
    }

    #[test]
    fn discovered_levels_prefer_medium_and_fall_back_to_the_templates_own() {
        let opt = |v: &str| EffortOption {
            name: pretty_level(v),
            value: v.to_string(),
        };
        // The real Qwen3 guard order — medium is offered, so medium wins over
        // the template's own (first, highest) default.
        let graded = [opt("xhigh"), opt("medium"), opt("low")];
        assert_eq!(preferred_default(&graded), "medium");

        // A model whose template names no medium keeps the old behaviour:
        // its first, highest level.
        let no_medium = [opt("xhigh"), opt("low")];
        assert_eq!(preferred_default(&no_medium), "xhigh");

        // Degenerate: no levels at all (the caller hides the dropdown before
        // this can matter, but it must not panic).
        assert_eq!(preferred_default(&[]), "");
    }

    #[test]
    fn pretty_level_names_are_readable() {
        assert_eq!(pretty_level("xhigh"), "Extra high");
        assert_eq!(pretty_level("medium"), "Medium");
        assert_eq!(pretty_level("ultra"), "Ultra"); // unknown → title-cased
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
    fn self_hosted_gets_graded_off_low_medium_high() {
        let opts = effort_options(EffortDialect::LlamaServerThinking);
        assert_eq!(opts.len(), 4);
        assert_eq!(opts[0].value, "off");
        assert_eq!(opts[1].value, "low");
        assert_eq!(opts[2].value, "medium");
        assert_eq!(opts[3].value, "high");
    }
}
