//! Detecting a card's reasoning-effort ladder.
//!
//! One question per card — "which effort levels does this model take, and can
//! reasoning be switched off?" — answered from the most direct source that
//! answers at all, per provider type:
//!
//! | type                    | sources, in order                                   |
//! |-------------------------|-----------------------------------------------------|
//! | `openrouter`            | OpenRouter catalog                                  |
//! | `anthropic`             | Models API `capabilities`, then the catalog         |
//! | `openai`                | catalog                                             |
//! | `fireworks`/`deepinfra` | catalog                                             |
//! | `custom_openai`/`ollama`| `/props` template, HF template, catalog by HF id, Ollama `/api/show` |
//!
//! and, failing all of those, the old name patterns (hosted types only).
//!
//! Deliberately no probe requests: a probe is a real model call billed to the
//! user's key, and Kitty never calls the model itself — the engine does. A
//! level the model turns out to reject comes back from the engine as an
//! `effort_rejected` error instead, which marks the card for re-detection.

use serde_json::Value;

use super::ladder::{normalize_levels, EffortCaps, EffortSource, ON};
use crate::openrouter::catalog::{
    match_in_catalog, normalize_model_id, OpenRouterCatalogEntry, OpenRouterReasoning,
};

/// What a detection pass learned. Only the first two are facts about the
/// model; `Unknown` (nothing reachable, nothing matched) must never overwrite
/// what a card already has.
#[derive(Debug, Clone, PartialEq)]
pub enum Detection {
    Found(EffortCaps),
    Unknown,
}

/// Everything detection needs about a card, snapshotted out of the config so
/// no lock is held across the network calls.
#[derive(Debug, Clone)]
pub struct CardSnapshot {
    pub id: String,
    pub provider_type: String,
    pub base_url: String,
    pub model: String,
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

fn caps(levels: Vec<String>, can_disable: bool, source: EffortSource) -> EffortCaps {
    EffortCaps {
        levels,
        can_disable,
        anthropic_thinking: None,
        source,
        detected_at: now_unix(),
        stale: false,
    }
}

/// Run detection for one card.
pub async fn detect(
    card: &CardSnapshot,
    catalog: &[OpenRouterCatalogEntry],
    api_key: Option<&str>,
) -> Detection {
    let model = card.model.trim();
    if model.is_empty() || card.provider_type == "local" {
        return Detection::Unknown;
    }
    let found = match card.provider_type.as_str() {
        "openrouter" => exact_catalog_entry(model, catalog)
            .or_else(|| match_in_catalog(model, catalog))
            .and_then(|e| e.reasoning.as_ref())
            .map(from_openrouter),
        "anthropic" => detect_anthropic(card, catalog, api_key).await,
        "openai" | "fireworks" | "deepinfra" => match_in_catalog(model, catalog)
            .and_then(|e| e.reasoning.as_ref())
            .map(from_openrouter),
        "custom_openai" | "ollama" => detect_self_hosted(card, catalog, api_key).await,
        _ => None,
    };
    match found.or_else(|| heuristic(&card.provider_type, model)) {
        Some(c) => Detection::Found(c),
        None => Detection::Unknown,
    }
}

fn exact_catalog_entry<'a>(
    model: &str,
    catalog: &'a [OpenRouterCatalogEntry],
) -> Option<&'a OpenRouterCatalogEntry> {
    catalog.iter().find(|e| e.id.eq_ignore_ascii_case(model))
}

/// Map OpenRouter's `reasoning` object onto a ladder.
///
/// * `supported_efforts` (minus `none`) is the ladder.
/// * Off is offered when reasoning isn't `mandatory`, or `none` is listed.
/// * No efforts listed: an optional model is an on/off toggle; a mandatory
///   one has nothing to choose, so no control.
pub fn from_openrouter(r: &OpenRouterReasoning) -> EffortCaps {
    let levels = normalize_levels(&r.supported_efforts);
    let lists_none = r.supported_efforts.iter().any(|l| l == "none");
    let can_disable = !r.mandatory || lists_none;
    if levels.is_empty() {
        return if can_disable {
            caps(vec![ON.to_string()], true, EffortSource::OpenRouter)
        } else {
            caps(Vec::new(), false, EffortSource::OpenRouter)
        };
    }
    caps(levels, can_disable, EffortSource::OpenRouter)
}

// ---------------------------------------------------------------- Anthropic

/// What the Models API says about one Claude model. `None` when the response
/// carries no `capabilities` tree (an older API, or a proxy that strips it).
#[derive(Debug, Clone, PartialEq)]
pub struct AnthropicModelCaps {
    pub levels: Vec<String>,
    pub adaptive: bool,
    pub thinking: bool,
}

/// Parse `GET /v1/models/{id}`. The shape (verified against the API docs):
/// `capabilities.effort.{low,medium,high,xhigh,max}.supported` and
/// `capabilities.thinking.types.{adaptive,enabled}.supported`.
pub fn parse_anthropic_model(v: &Value) -> Option<AnthropicModelCaps> {
    let caps = v.get("capabilities")?;
    let supported = |node: Option<&Value>| {
        node.and_then(|n| n.get("supported"))
            .and_then(|s| s.as_bool())
            .unwrap_or(false)
    };
    let thinking = caps.get("thinking");
    let adaptive = supported(
        thinking
            .and_then(|t| t.get("types"))
            .and_then(|t| t.get("adaptive")),
    );
    let enabled = supported(
        thinking
            .and_then(|t| t.get("types"))
            .and_then(|t| t.get("enabled")),
    );
    let effort = caps.get("effort");
    let mut raw = Vec::new();
    if supported(effort) {
        for level in ["low", "medium", "high", "xhigh", "max"] {
            if supported(effort.and_then(|e| e.get(level))) {
                raw.push(level);
            }
        }
    }
    Some(AnthropicModelCaps {
        levels: normalize_levels(&raw),
        adaptive,
        thinking: supported(thinking) || adaptive || enabled,
    })
}

async fn detect_anthropic(
    card: &CardSnapshot,
    catalog: &[OpenRouterCatalogEntry],
    api_key: Option<&str>,
) -> Option<EffortCaps> {
    // Whether thinking can be switched off isn't in the Models API; take it
    // from the catalog (`claude-opus-5-5` matches `anthropic/claude-opus-5.5`
    // through `normalize_model_id`). With no match, Off is offered only on the
    // budget-style models, where omitting thinking has always meant off — a
    // missing option beats a 400 on the adaptive ones that refuse it.
    let catalog_entry = match_in_catalog(&card.model, catalog)
        .filter(|e| e.id.starts_with("anthropic/"))
        .and_then(|e| e.reasoning.as_ref());
    let api = match api_key.filter(|k| !k.is_empty()) {
        Some(key) => fetch_anthropic_model(&card.base_url, &card.model, key).await,
        None => None,
    };
    match api {
        Some(m) if m.thinking => {
            let adaptive = m.adaptive;
            let levels = if m.levels.is_empty() {
                // Budget-only models take no `effort`; the engine maps the
                // three standard levels onto token budgets.
                vec!["low".into(), "medium".into(), "high".into()]
            } else {
                m.levels
            };
            let can_disable = match catalog_entry {
                Some(r) => !r.mandatory || r.supported_efforts.iter().any(|l| l == "none"),
                None => !adaptive,
            };
            let mut c = caps(levels, can_disable, EffortSource::ProviderApi);
            c.anthropic_thinking = Some(if adaptive { "adaptive" } else { "budget" }.into());
            Some(c)
        }
        Some(_) => Some(caps(Vec::new(), false, EffortSource::ProviderApi)),
        None => catalog_entry.map(|r| {
            let mut c = from_openrouter(r);
            // A catalog ladder above `high` only exists on adaptive models.
            let adaptive = c.levels.iter().any(|l| l == "xhigh" || l == "max");
            c.anthropic_thinking = Some(if adaptive { "adaptive" } else { "budget" }.into());
            c
        }),
    }
}

async fn fetch_anthropic_model(
    base_url: &str,
    model: &str,
    key: &str,
) -> Option<AnthropicModelCaps> {
    let base = base_url.trim().trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    let base = if base.is_empty() {
        "https://api.anthropic.com"
    } else {
        base
    };
    let url = format!("{base}/v1/models/{model}");
    let resp = crate::util::http_client()
        .get(&url)
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        tracing::debug!(status = %resp.status(), url, "Anthropic model lookup failed");
        return None;
    }
    let v: Value = resp.json().await.ok()?;
    parse_anthropic_model(&v)
}

// --------------------------------------------------------------- self-hosted

/// Pull the ordered reasoning-effort levels out of a chat template that guards
/// them, e.g. Qwen3's
/// `{%- if resolved_reasoning_effort not in ('xhigh', 'medium', 'low') %}`.
/// Returns the levels in template order, or `None` if the template has no
/// such guard (i.e. the model has no graded effort control).
///
/// Deliberately hand-parsed rather than regex to avoid pulling a regex
/// dependency into this crate for one pattern.
pub fn extract_effort_levels(template: &str) -> Option<Vec<String>> {
    let anchor = template.find("reasoning_effort")?;
    let after = &template[anchor..];
    let not_in = after.find("not in")?;
    let rest = &after[not_in..];
    let open = rest.find('(')?;
    let close = rest[open..].find(')')? + open;
    let inside = &rest[open + 1..close];
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

/// The ladder one chat template implies: the graded guard if present, else a
/// plain on/off toggle when the template reads `enable_thinking`, else `None`
/// (genuinely no thinking control). Off is offered whenever the template has
/// the `enable_thinking` toggle the engine sets to false for Off.
///
/// A boolean-only template used to be offered Low/Medium/High, all of which
/// read identically on the wire — three choices that did the same thing. It
/// is now the honest Off/On.
pub fn caps_from_template(template: &str, source: EffortSource) -> Option<EffortCaps> {
    let toggle = template.contains("enable_thinking");
    match extract_effort_levels(template) {
        Some(levels) => Some(caps(normalize_levels(&levels), toggle, source)),
        None if toggle => Some(caps(vec![ON.to_string()], true, source)),
        None => None,
    }
}

/// Normalize a provider base URL to an origin we can hang `/props` off: add a
/// scheme if the user typed a bare `host:port`, drop a trailing slash, and
/// drop a trailing `/v1` (llama-server serves `/props` at the root; LM
/// Studio's default base URL ends in `/v1`).
pub fn normalize_base(base_url: &str) -> String {
    let b = base_url.trim().trim_end_matches('/');
    let b = b.strip_suffix("/v1").unwrap_or(b);
    if b.contains("://") {
        b.to_string()
    } else {
        format!("http://{b}")
    }
}

/// The `owner/repo` HuggingFace id inside a model name, if there is a
/// plausible one. Local ids pick up decoration — an Ollama-style `:tag`, a
/// `.gguf` extension, a nested path inside a quantized repo — so strip those
/// and take the first two segments.
pub fn hf_repo_id(model: &str) -> Option<String> {
    let m = model.trim();
    if m.is_empty() || m.contains(' ') || m.starts_with('/') || m.contains('\\') {
        return None;
    }
    let m = m.split(':').next().unwrap_or(m);
    let m = m.strip_suffix(".gguf").unwrap_or(m);
    let mut parts = m.split('/').filter(|p| !p.is_empty());
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.len() < 2 {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// A catalog entry for a self-hosted model: by HuggingFace id first (a
/// quantized `unsloth/Qwen3.8-27B-GGUF` still names `Qwen3.8-27B`), then the
/// usual normalized-name match.
fn catalog_for_self_hosted<'a>(
    model: &str,
    catalog: &'a [OpenRouterCatalogEntry],
) -> Option<&'a OpenRouterCatalogEntry> {
    let repo_name = hf_repo_id(model)
        .and_then(|r| r.split('/').nth(1).map(str::to_string))
        .unwrap_or_else(|| model.to_string());
    let strip_quant = |s: &str| {
        let n = normalize_model_id(s);
        n.trim_end_matches("-gguf").to_string()
    };
    let needle = strip_quant(&repo_name);
    catalog
        .iter()
        .find(|e| {
            e.hugging_face_id
                .as_deref()
                .is_some_and(|h| strip_quant(h) == needle)
        })
        .or_else(|| match_in_catalog(&needle, catalog))
}

async fn detect_self_hosted(
    card: &CardSnapshot,
    catalog: &[OpenRouterCatalogEntry],
    api_key: Option<&str>,
) -> Option<EffortCaps> {
    let client = crate::util::http_client();

    // 1. The endpoint's own chat template — what the server actually enforces.
    let props_url = format!("{}/props", normalize_base(&card.base_url));
    let mut req = client
        .get(&props_url)
        .timeout(std::time::Duration::from_secs(8));
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        req = req.bearer_auth(key);
    }
    let mut read_a_template = false;
    if let Ok(resp) = req.send().await {
        if let Ok(v) = resp.json::<Value>().await {
            if let Some(tmpl) = v.get("chat_template").and_then(|t| t.as_str()) {
                read_a_template = true;
                if let Some(c) = caps_from_template(tmpl, EffortSource::ChatTemplate) {
                    return Some(c);
                }
            }
        }
    }

    // 2. HuggingFace, when the model id looks like `owner/repo`.
    if let Some(repo) = hf_repo_id(&card.model) {
        let hf_url = format!("https://huggingface.co/{repo}/resolve/main/tokenizer_config.json");
        if let Ok(resp) = client
            .get(&hf_url)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            if let Ok(v) = resp.json::<Value>().await {
                if let Some(tmpl) = v.get("chat_template").and_then(|t| t.as_str()) {
                    read_a_template = true;
                    if let Some(c) = caps_from_template(tmpl, EffortSource::HuggingFace) {
                        return Some(c);
                    }
                }
            }
        }
    }

    // 3. Ollama says whether the model thinks at all.
    if card.provider_type == "ollama" {
        if let Some(thinks) = ollama_thinks(&card.base_url, &card.model).await {
            return Some(if thinks {
                caps(vec![ON.to_string()], true, EffortSource::Ollama)
            } else {
                caps(Vec::new(), false, EffortSource::Ollama)
            });
        }
    }

    // A template was read and has no control: that is a definite answer, and
    // the catalog (which describes the hosted build) must not override it.
    if read_a_template {
        return Some(caps(Vec::new(), false, EffortSource::ChatTemplate));
    }

    // 4. The OpenRouter entry for the same open-weights model.
    if let Some(c) = catalog_for_self_hosted(&card.model, catalog)
        .and_then(|e| e.reasoning.as_ref())
        .map(from_openrouter)
    {
        return Some(c);
    }

    tracing::warn!(
        props_url,
        model = card.model,
        "could not read a chat template for this endpoint, so its reasoning levels are \
         unknown. llama-server serves /props at the root; LM Studio serves it nowhere, in \
         which case naming the model as an owner/repo id lets its template be read from \
         HuggingFace instead"
    );
    None
}

async fn ollama_thinks(base_url: &str, model: &str) -> Option<bool> {
    let url = format!("{}/api/show", normalize_base(base_url));
    let resp = crate::util::http_client()
        .post(url)
        .json(&serde_json::json!({ "model": model }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    let v: Value = resp.json().await.ok()?;
    thinks_from_show(&v)
}

/// Whether an Ollama `/api/show` response lists the `thinking` capability.
/// `None` when the array is absent (an older Ollama), which is not a "no".
pub fn thinks_from_show(v: &Value) -> Option<bool> {
    let caps = v.get("capabilities")?.as_array()?;
    Some(
        caps.iter()
            .filter_map(|c| c.as_str())
            .any(|c| c.eq_ignore_ascii_case("thinking")),
    )
}

// ----------------------------------------------------------------- fallback

/// The pre-detection name patterns, kept as the last resort for hosted
/// types when neither the provider nor the catalog answers.
pub fn heuristic(provider_type: &str, model: &str) -> Option<EffortCaps> {
    let lmh = || vec!["low".to_string(), "medium".to_string(), "high".to_string()];
    match provider_type {
        "anthropic" if anthropic_takes_effort(model) => {
            let mut c = caps(lmh(), true, EffortSource::Heuristic);
            c.anthropic_thinking = Some("budget".into());
            Some(c)
        }
        "openai" if openai_takes_effort(model) => Some(caps(lmh(), false, EffortSource::Heuristic)),
        "openrouter" if openrouter_takes_effort(model) => {
            Some(caps(lmh(), true, EffortSource::Heuristic))
        }
        _ => None,
    }
}

/// `claude-*` minus the families that predate extended thinking.
fn anthropic_takes_effort(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    if !m.contains("claude") {
        return false;
    }
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

/// OpenAI's reasoning families: the o-series and `gpt-5*`.
fn openai_takes_effort(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    let bare = m.rsplit('/').next().unwrap_or(&m);
    if bare.starts_with("gpt-5") {
        return true;
    }
    ["o1", "o2", "o3", "o4"]
        .iter()
        .any(|n| bare == *n || bare.starts_with(&format!("{n}-")))
}

fn openrouter_takes_effort(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    let bare = m.rsplit('/').next().unwrap_or(&m);
    openai_takes_effort(bare)
        || anthropic_takes_effort(bare)
        || bare.contains("deepseek-r1")
        || (bare.contains("qwen3") && bare.contains("thinking"))
        || (bare.contains("grok") && (bare.contains("mini") || bare.contains("reasoning")))
        || bare.contains("gemini-2.5")
        || bare.contains("glm-4")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(efforts: &[&str], mandatory: bool) -> OpenRouterReasoning {
        OpenRouterReasoning {
            supported_efforts: efforts.iter().map(|s| s.to_string()).collect(),
            default_effort: None,
            mandatory,
        }
    }

    #[test]
    fn openrouter_shapes() {
        // Claude Haiku 5.5: optional, five levels.
        let c = from_openrouter(&r(
            &["max", "xhigh", "high", "medium", "low", "none"],
            false,
        ));
        assert_eq!(c.levels, ["low", "medium", "high", "xhigh", "max"]);
        assert!(c.can_disable);
        // Claude Sonnet 5.5: mandatory — no Off.
        let c = from_openrouter(&r(&["max", "xhigh", "high", "medium", "low"], true));
        assert!(!c.can_disable);
        assert_eq!(c.levels.len(), 5);
        // Toggle-only.
        let c = from_openrouter(&r(&[], false));
        assert_eq!(c.levels, ["on"]);
        assert!(c.can_disable);
        // Mandatory with nothing to pick: no control.
        assert!(!from_openrouter(&r(&[], true)).has_control());
    }

    #[test]
    fn anthropic_capabilities_parse() {
        let v = serde_json::json!({
            "id": "claude-opus-5-5",
            "capabilities": {
                "thinking": {"supported": true, "types": {
                    "enabled": {"supported": false}, "adaptive": {"supported": true}}},
                "effort": {"supported": true, "low": {"supported": true},
                    "medium": {"supported": true}, "high": {"supported": true},
                    "xhigh": {"supported": true}, "max": {"supported": true}}
            }
        });
        let m = parse_anthropic_model(&v).unwrap();
        assert!(m.adaptive && m.thinking);
        assert_eq!(m.levels, ["low", "medium", "high", "xhigh", "max"]);

        let haiku45 = serde_json::json!({
            "capabilities": {
                "thinking": {"supported": true, "types": {
                    "enabled": {"supported": true}, "adaptive": {"supported": false}}},
                "effort": {"supported": false}
            }
        });
        let m = parse_anthropic_model(&haiku45).unwrap();
        assert!(!m.adaptive && m.thinking && m.levels.is_empty());
        assert!(parse_anthropic_model(&serde_json::json!({"id": "x"})).is_none());
    }

    #[test]
    fn templates() {
        let graded = "{% if reasoning_effort not in ('xhigh', 'medium', 'low') %}\
                      {% set enable_thinking = false %}{% endif %}";
        let c = caps_from_template(graded, EffortSource::ChatTemplate).unwrap();
        assert_eq!(c.levels, ["low", "medium", "xhigh"]);
        assert!(c.can_disable);

        let boolean_only = "{% if enable_thinking %}<think>{% endif %}";
        let c = caps_from_template(boolean_only, EffortSource::ChatTemplate).unwrap();
        assert_eq!(c.levels, ["on"]);
        assert!(c.can_disable);

        assert!(caps_from_template("{{ messages }}", EffortSource::ChatTemplate).is_none());
    }

    #[test]
    fn normalize_base_strips_a_trailing_v1() {
        for (input, want) in [
            ("http://localhost:1234/v1", "http://localhost:1234"),
            ("http://localhost:1234/v1/", "http://localhost:1234"),
            ("localhost:1234/v1", "http://localhost:1234"),
            ("http://localhost:1234/v10", "http://localhost:1234/v10"),
        ] {
            assert_eq!(normalize_base(input), want, "input {input:?}");
        }
    }

    #[test]
    fn hf_repo_id_tolerates_quant_tags_and_extensions() {
        for (input, want) in [
            ("Qwen/Qwen3-8B", Some("Qwen/Qwen3-8B")),
            ("Qwen/Qwen3-8B-GGUF:Q5_K_M", Some("Qwen/Qwen3-8B-GGUF")),
            ("Qwen/Qwen3-8B-GGUF/model.gguf", Some("Qwen/Qwen3-8B-GGUF")),
            ("qwen3-8b", None),
            ("", None),
        ] {
            assert_eq!(hf_repo_id(input).as_deref(), want, "input {input:?}");
        }
    }

    fn entry(
        id: &str,
        hf: Option<&str>,
        reasoning: Option<OpenRouterReasoning>,
    ) -> OpenRouterCatalogEntry {
        OpenRouterCatalogEntry {
            id: id.into(),
            name: id.into(),
            created: None,
            context_length: None,
            pricing_prompt: None,
            pricing_completion: None,
            intelligence_index: None,
            coding_index: None,
            agentic_index: None,
            accepts_images: None,
            supports_tools: None,
            reasoning,
            hugging_face_id: hf.map(String::from),
            price_rank: None,
            cost_tier: None,
        }
    }

    #[test]
    fn a_quantized_self_hosted_model_finds_its_catalog_entry_by_hf_id() {
        let cat = vec![
            entry("other/model", None, None),
            entry(
                "qwen/qwen3.8-27b",
                Some("Qwen/Qwen3.8-27B"),
                Some(r(&["xhigh", "medium", "low", "none"], false)),
            ),
        ];
        let e = catalog_for_self_hosted("unsloth/Qwen3.8-27B-GGUF:Q4_K_M", &cat).unwrap();
        assert_eq!(e.id, "qwen/qwen3.8-27b");
    }

    #[test]
    fn ollama_thinking_capability() {
        assert_eq!(
            thinks_from_show(&serde_json::json!({"capabilities": ["completion", "thinking"]})),
            Some(true)
        );
        assert_eq!(
            thinks_from_show(&serde_json::json!({"capabilities": ["completion"]})),
            Some(false)
        );
        assert_eq!(thinks_from_show(&serde_json::json!({})), None);
    }

    #[test]
    fn heuristics_survive_as_a_fallback() {
        assert!(heuristic("openai", "o3-mini").is_some());
        assert!(heuristic("openai", "gpt-4o").is_none());
        assert!(heuristic("anthropic", "claude-3-5-sonnet").is_none());
        assert!(heuristic("openrouter", "meta-llama/llama-3.3-70b").is_none());
        assert!(heuristic("custom_openai", "qwen3").is_none());
        assert!(!heuristic("openai", "o3").unwrap().can_disable);
    }
}
