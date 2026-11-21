//! The models a provider offers, described with what OpenRouter's public
//! catalog knows about them (release date, output price, Artificial Analysis
//! intelligence index, image input), ordered so the strongest recent models
//! come first, and marked `$`/`$$`/`$$$` by price. Ported from Bacon.
//!
//! Every list of models Kitty shows is in this order: a provider's checklist
//! (Settings → Providers), the default-model menu, and the model menus of a
//! chat, a scheduled task and a specialist.
//!
//! [`defaults`] picks the default model when the first provider is added.

pub mod defaults;

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::config::providers::ProviderProfile;
use crate::openrouter::catalog::{match_in_catalog, OpenRouterCatalogEntry};
use crate::state::AppState;

const SECS_PER_MONTH: f64 = 30.44 * 24.0 * 60.0 * 60.0;
/// Age stops counting against a model after this many months.
const MAX_AGE_PENALTY_MONTHS: f64 = 24.0;

/// One model at one provider, as the settings and menus show it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ModelInfo {
    pub provider_id: String,
    pub provider_name: String,
    pub provider_type: String,
    /// The id the provider uses (what is sent to it).
    pub model: String,
    /// The setup this describes, when it describes one (`list_providers`).
    pub card_id: Option<String>,
    /// A readable name: the catalog's (vendor prefix dropped for a
    /// single-company provider), else the id.
    pub name: String,
    /// Checked: in Kitty's model list.
    pub enabled: bool,
    /// Reads images: the catalog says so, the user marked it, or the provider
    /// is Anthropic (every current Claude model does).
    pub vision: bool,
    /// Whether the catalog knew this model (and so whether `vision` came
    /// from it rather than from the user's mark).
    pub in_catalog: bool,
    /// Unix seconds.
    pub created: Option<i64>,
    /// $ per million output tokens.
    pub price_out_per_m: Option<f64>,
    /// $ per million input tokens.
    pub price_in_per_m: Option<f64>,
    /// `$`, `$$`, `$$$`, `free`, or `None` when the price is not known.
    pub price_tier: Option<&'static str>,
    pub intelligence: Option<f64>,
    pub context_length: Option<u32>,
}

/// `$` up to $3 per million output tokens, `$$` up to $8, `$$$` above.
pub fn price_tier(price_out_per_m: Option<f64>) -> Option<&'static str> {
    let p = price_out_per_m?;
    if p < 0.0 || p.is_nan() {
        return None;
    }
    Some(if p == 0.0 {
        "free"
    } else if p <= 3.0 {
        "$"
    } else if p <= 8.0 {
        "$$"
    } else {
        "$$$"
    })
}

/// A single-company provider: every model it lists comes from one lab.
pub fn single_company(provider_type: &str, base_url: &str) -> bool {
    provider_type == "anthropic"
        || (provider_type == "openai" && base_url.to_ascii_lowercase().contains("api.openai.com"))
}

/// The ranking score: intelligence, less a point per month of age (capped).
fn score(m: &ModelInfo, now: i64) -> Option<f64> {
    let intelligence = m.intelligence?;
    let age = m
        .created
        .map(|c| ((now - c).max(0) as f64 / SECS_PER_MONTH).min(MAX_AGE_PENALTY_MONTHS))
        .unwrap_or(MAX_AGE_PENALTY_MONTHS);
    Some(intelligence - age)
}

/// Order models for a list: those with an intelligence index first, by
/// score (ties → newer, then cheaper); then the rest, newest first; models
/// with no date last, by name.
pub fn rank(models: &mut [ModelInfo], now: i64) {
    use std::cmp::Ordering;
    let cmp_f = |a: Option<f64>, b: Option<f64>| -> Ordering {
        match (a, b) {
            (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
            (Some(_), None) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (None, None) => Ordering::Equal,
        }
    };
    models.sort_by(|a, b| {
        // Descending score, then descending date, then ascending price, name.
        cmp_f(score(b, now), score(a, now))
            .then_with(|| cmp_f(b.created.map(|c| c as f64), a.created.map(|c| c as f64)))
            .then_with(|| cmp_f(b.price_out_per_m.map(|p| -p), a.price_out_per_m.map(|p| -p)))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// The catalog's name, without its vendor prefix ("Anthropic: ") for a
/// single-company provider; the id when the catalog does not know it.
fn display_name(
    provider: &ProviderProfile,
    model: &str,
    entry: Option<&OpenRouterCatalogEntry>,
) -> String {
    let Some(entry) = entry else {
        return model.to_string();
    };
    if single_company(&provider.provider_type, &provider.base_url) {
        if let Some((_, rest)) = entry.name.split_once(": ") {
            return rest.to_string();
        }
    }
    entry.name.clone()
}

/// Describe one provider model against the catalog.
pub fn describe(
    provider: &ProviderProfile,
    model: &str,
    catalog: &[OpenRouterCatalogEntry],
) -> ModelInfo {
    let entry = match_in_catalog(model, catalog);
    let per_m = |p: Option<f64>| p.map(|p| p * 1_000_000.0);
    let price = per_m(entry.and_then(|e| e.pricing_completion));
    let vision = provider.vision_models.iter().any(|m| m == model)
        || provider.provider_type == "anthropic"
        || entry.and_then(|e| e.accepts_images).unwrap_or(false);
    ModelInfo {
        provider_id: provider.id.clone(),
        provider_name: provider.name.clone(),
        provider_type: provider.provider_type.clone(),
        model: model.to_string(),
        card_id: None,
        name: display_name(provider, model, entry),
        enabled: provider.is_enabled(model),
        vision,
        in_catalog: entry.is_some(),
        created: entry.and_then(|e| e.created),
        price_out_per_m: price,
        price_in_per_m: per_m(entry.and_then(|e| e.pricing_prompt)),
        price_tier: price_tier(price),
        intelligence: entry.and_then(|e| e.intelligence_index),
        context_length: entry.and_then(|e| e.context_length),
    }
}

/// Every listed model of one provider (checked or not), ranked.
pub fn provider_models(
    provider: &ProviderProfile,
    catalog: &[OpenRouterCatalogEntry],
    now: i64,
) -> Vec<ModelInfo> {
    let mut out: Vec<ModelInfo> = provider
        .models
        .iter()
        .map(|m| describe(provider, m, catalog))
        .collect();
    rank(&mut out, now);
    out
}

/// The cached catalog's entries (refreshed first when stale or missing; a
/// failed refresh keeps whatever was cached).
pub async fn catalog(app: &AppHandle) -> Vec<OpenRouterCatalogEntry> {
    let state = app.state::<AppState>();
    crate::openrouter::catalog::ensure_catalog_fresh(&state).await;
    cached_catalog(app)
}

/// What the catalog holds right now, without refreshing.
pub fn cached_catalog(app: &AppHandle) -> Vec<OpenRouterCatalogEntry> {
    let state = app.state::<AppState>();
    let guard = state.openrouter_catalog.lock().unwrap();
    guard
        .as_ref()
        .map(|c| c.entries.clone())
        .unwrap_or_default()
}

pub fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// A provider's whole model list (unchecked ones too), described and ranked,
/// for the checklist in its form. `profile` is the form's draft: a fresh
/// fetch not saved yet is described as it stands.
#[tauri::command]
pub async fn describe_provider_models(
    app: AppHandle,
    profile: ProviderProfile,
) -> Result<Vec<ModelInfo>, String> {
    let catalog = catalog(&app).await;
    Ok(provider_models(&profile, &catalog, now_unix()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const NOW: i64 = 1_800_000_000;
    pub const MONTH: i64 = SECS_PER_MONTH as i64;

    pub fn entry(
        id: &str,
        name: &str,
        months_old: i64,
        out_per_m: f64,
        intelligence: Option<f64>,
        images: bool,
    ) -> OpenRouterCatalogEntry {
        OpenRouterCatalogEntry {
            id: id.into(),
            name: name.into(),
            created: Some(NOW - months_old * MONTH),
            context_length: None,
            pricing_prompt: Some(out_per_m / 5.0 / 1_000_000.0),
            pricing_completion: Some(out_per_m / 1_000_000.0),
            intelligence_index: intelligence,
            coding_index: None,
            agentic_index: None,
            accepts_images: Some(images),
            supports_tools: Some(true),
            reasoning: None,
            hugging_face_id: None,
            price_rank: None,
            cost_tier: None,
        }
    }

    pub fn provider(id: &str, kind: &str, base: &str, models: &[&str]) -> ProviderProfile {
        let mut p: ProviderProfile = serde_json::from_value(serde_json::json!({
            "id": id, "name": id.to_uppercase(), "provider_type": kind,
            "base_url": base, "models": models,
        }))
        .unwrap();
        crate::config::providers::upgrade_to_model_lists(std::slice::from_mut(&mut p));
        p
    }

    #[test]
    fn price_tiers_break_at_three_and_eight_dollars() {
        assert_eq!(price_tier(None), None);
        assert_eq!(price_tier(Some(0.0)), Some("free"));
        assert_eq!(price_tier(Some(0.4)), Some("$"));
        assert_eq!(price_tier(Some(3.0)), Some("$"));
        assert_eq!(price_tier(Some(3.01)), Some("$$"));
        assert_eq!(price_tier(Some(8.0)), Some("$$"));
        assert_eq!(price_tier(Some(8.01)), Some("$$$"));
        assert_eq!(price_tier(Some(75.0)), Some("$$$"));
    }

    fn info(
        name: &str,
        intelligence: Option<f64>,
        months_old: Option<i64>,
        price: Option<f64>,
    ) -> ModelInfo {
        ModelInfo {
            provider_id: "p".into(),
            provider_name: "P".into(),
            provider_type: "openrouter".into(),
            model: name.into(),
            card_id: None,
            name: name.into(),
            enabled: true,
            vision: false,
            in_catalog: intelligence.is_some(),
            created: months_old.map(|m| NOW - m * MONTH),
            price_out_per_m: price,
            price_in_per_m: None,
            price_tier: price_tier(price),
            intelligence,
            context_length: None,
        }
    }

    fn names(models: &[ModelInfo]) -> Vec<&str> {
        models.iter().map(|m| m.name.as_str()).collect()
    }

    #[test]
    fn ranking_weighs_intelligence_against_age() {
        let mut models = vec![
            // 70 − 12 = 58
            info("old-smart", Some(70.0), Some(12), Some(10.0)),
            // 62 − 1 = 61
            info("new-good", Some(62.0), Some(1), Some(10.0)),
            // 80 − 24 (capped; really 40 months old) = 56
            info("ancient-genius", Some(80.0), Some(40), Some(10.0)),
            info("unrated-new", None, Some(0), Some(1.0)),
            info("unrated-old", None, Some(30), Some(1.0)),
            info("b-undated", None, None, None),
            info("a-undated", None, None, None),
        ];
        rank(&mut models, NOW);
        assert_eq!(
            names(&models),
            [
                "new-good",
                "old-smart",
                "ancient-genius",
                "unrated-new",
                "unrated-old",
                "a-undated",
                "b-undated"
            ]
        );
    }

    #[test]
    fn equal_scores_go_to_the_newer_then_the_cheaper() {
        let mut models = vec![
            info("older", Some(61.0), Some(2), Some(1.0)),
            info("pricey", Some(60.0), Some(1), Some(20.0)),
            info("cheap", Some(60.0), Some(1), Some(2.0)),
        ];
        rank(&mut models, NOW);
        assert_eq!(names(&models), ["cheap", "pricey", "older"]);
    }

    #[test]
    fn a_model_is_described_from_the_catalog() {
        let catalog = vec![entry(
            "anthropic/claude-sonnet-5",
            "Anthropic: Claude Sonnet 5",
            2,
            15.0,
            Some(65.0),
            true,
        )];
        let p = provider(
            "a",
            "anthropic",
            "https://api.anthropic.com",
            &["claude-sonnet-5-20260101"],
        );
        let m = describe(&p, "claude-sonnet-5-20260101", &catalog);
        assert_eq!(m.name, "Claude Sonnet 5");
        assert_eq!(m.price_tier, Some("$$$"));
        assert_eq!(m.price_in_per_m, Some(3.0));
        assert_eq!(m.intelligence, Some(65.0));
        assert_eq!(m.card_id, None, "set only where it describes a setup");
        assert!(m.vision && m.in_catalog && m.enabled);

        let or = provider(
            "o",
            "openrouter",
            "https://openrouter.ai/api/v1",
            &["anthropic/claude-sonnet-5"],
        );
        assert_eq!(
            describe(&or, "anthropic/claude-sonnet-5", &catalog).name,
            "Anthropic: Claude Sonnet 5"
        );
        // Unknown to the catalog: the id, no price, vision only by the mark.
        let mut local = provider("l", "custom_openai", "http://box", &["my-finetune"]);
        let m = describe(&local, "my-finetune", &catalog);
        assert_eq!(
            (m.name.as_str(), m.price_tier, m.vision),
            ("my-finetune", None, false)
        );
        local.vision_models = vec!["my-finetune".into()];
        assert!(describe(&local, "my-finetune", &catalog).vision);
    }

    #[test]
    fn an_unchecked_model_is_listed_but_not_enabled() {
        let mut p = provider("o", "openrouter", "x", &["a", "b"]);
        p.enabled_models = vec!["a".into()];
        let models = provider_models(&p, &[], NOW);
        assert_eq!(models.len(), 2);
        assert!(models.iter().find(|m| m.model == "a").unwrap().enabled);
        assert!(!models.iter().find(|m| m.model == "b").unwrap().enabled);
    }
}
