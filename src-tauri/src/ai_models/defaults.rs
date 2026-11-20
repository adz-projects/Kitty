//! The model a provider suggests: the default model chosen when the first
//! provider is added (or the default was lost), and the one model checked
//! for a fresh fetch of a long list. Ported from Bacon's main-use pick.
//!
//! * **Single-company providers** (Anthropic; OpenAI at api.openai.com):
//!   recent models (the last 12 months) are grouped into price tiers, and
//!   the middle tier's best is picked (Haiku / Sonnet / Opus → Sonnet; with
//!   two tiers, the upper).
//! * **Multi-vendor providers** (OpenRouter, Fireworks, DeepInfra,
//!   self-hosted): by OpenRouter's catalog, the most intelligent model
//!   released in the last 3 months under $5 per million output tokens. When
//!   nothing qualifies the window widens (6 months, a year, any time); then
//!   unpriced models are allowed; then the first model.

use tauri::AppHandle;

use super::{single_company, ModelInfo};
use crate::config::providers::ProviderProfile;

const DAY: i64 = 24 * 60 * 60;
/// Single-company providers: models older than this are not candidates.
const RECENT_DAYS: i64 = 365;
/// Multi-vendor providers: the release windows tried in turn (`None` = any).
const WINDOWS: [Option<i64>; 4] = [Some(90), Some(180), Some(365), None];
/// $ per million output tokens.
const MAX_PRICE: f64 = 5.0;

/// The suggested model among `models` (described, `super::describe`); only
/// checked ones when `checked_only`.
pub fn pick(
    provider_type: &str,
    base_url: &str,
    models: &[ModelInfo],
    checked_only: bool,
    now: i64,
) -> Option<String> {
    let candidates: Vec<&ModelInfo> = models
        .iter()
        .filter(|m| !checked_only || m.enabled)
        .collect();
    let picked = if single_company(provider_type, base_url) {
        pick_single(&candidates, now)
    } else {
        pick_multi(&candidates, now)
    };
    picked
        .or_else(|| candidates.first().copied())
        .map(|m| m.model.clone())
}

/// Higher intelligence, then cheaper, then newer.
fn best<'a>(candidates: impl IntoIterator<Item = &'a ModelInfo>) -> Option<&'a ModelInfo> {
    let intelligence = |m: &ModelInfo| m.intelligence.unwrap_or(f64::MIN);
    let price = |m: &ModelInfo| m.price_out_per_m.unwrap_or(f64::MAX);
    candidates.into_iter().max_by(|a, b| {
        intelligence(a)
            .total_cmp(&intelligence(b))
            .then_with(|| price(b).total_cmp(&price(a)))
            .then_with(|| a.created.unwrap_or(0).cmp(&b.created.unwrap_or(0)))
    })
}

fn is_free_variant(m: &ModelInfo) -> bool {
    // OpenRouter's rate-limited free copies are no one's default.
    m.model.ends_with(":free") || m.price_out_per_m == Some(0.0)
}

fn pick_single<'a>(candidates: &[&'a ModelInfo], now: i64) -> Option<&'a ModelInfo> {
    let recent: Vec<&ModelInfo> = candidates
        .iter()
        .copied()
        .filter(|m| m.in_catalog && !is_free_variant(m))
        .filter(|m| m.price_out_per_m.is_some())
        .filter(|m| m.created.map_or(true, |c| now - c <= RECENT_DAYS * DAY))
        .collect();
    // Price tiers, cheapest first (prices to the cent).
    let cents = |m: &ModelInfo| m.price_out_per_m.map(|p| (p * 100.0).round() as i64);
    let mut tiers: Vec<i64> = recent.iter().filter_map(|m| cents(m)).collect();
    tiers.sort_unstable();
    tiers.dedup();
    // The middle tier; with an even count, the upper of the two middles.
    let tier = *tiers.get(tiers.len() / 2)?;
    best(recent.iter().copied().filter(|m| cents(m) == Some(tier)))
}

fn pick_multi<'a>(candidates: &[&'a ModelInfo], now: i64) -> Option<&'a ModelInfo> {
    let rated: Vec<&ModelInfo> = candidates
        .iter()
        .copied()
        .filter(|m| m.intelligence.is_some() && !is_free_variant(m))
        .collect();
    let within = |m: &ModelInfo, days: Option<i64>| {
        days.map_or(true, |d| m.created.is_some_and(|c| now - c <= d * DAY))
    };
    let under = |m: &ModelInfo| m.price_out_per_m.is_some_and(|p| p < MAX_PRICE);
    WINDOWS
        .iter()
        .find_map(|days| {
            best(
                rated
                    .iter()
                    .copied()
                    .filter(|m| within(m, *days) && under(m)),
            )
        })
        // Nothing priced fits (a self-hosted server): price is no limit.
        .or_else(|| {
            best(
                rated
                    .iter()
                    .copied()
                    .filter(|m| m.price_out_per_m.is_none()),
            )
        })
}

/// The model a provider's form should suggest checking: [`pick`] over every
/// listed model, checked or not.
#[tauri::command]
pub async fn suggest_provider_model(
    app: AppHandle,
    profile: ProviderProfile,
) -> Result<Option<String>, String> {
    let catalog = super::catalog(&app).await;
    let now = super::now_unix();
    let models = super::provider_models(&profile, &catalog, now);
    Ok(pick(
        &profile.provider_type,
        &profile.base_url,
        &models,
        false,
        now,
    ))
}

#[cfg(test)]
mod tests {
    use super::super::provider_models;
    use super::super::tests::{entry, provider, NOW};
    use super::*;

    fn picked(
        p: &ProviderProfile,
        catalog: &[crate::openrouter::catalog::OpenRouterCatalogEntry],
    ) -> Option<String> {
        let models = provider_models(p, catalog, NOW);
        pick(&p.provider_type, &p.base_url, &models, true, NOW)
    }

    #[test]
    fn anthropic_gets_the_middle_tier() {
        let catalog = vec![
            entry(
                "anthropic/claude-haiku-5",
                "Anthropic: Claude Haiku 5",
                3,
                5.0,
                Some(50.0),
                true,
            ),
            entry(
                "anthropic/claude-sonnet-5",
                "Anthropic: Claude Sonnet 5",
                2,
                15.0,
                Some(65.0),
                true,
            ),
            entry(
                "anthropic/claude-opus-5",
                "Anthropic: Claude Opus 5",
                2,
                25.0,
                Some(72.0),
                true,
            ),
            // Last generation, and too old.
            entry(
                "anthropic/claude-3-haiku",
                "Anthropic: Claude 3 Haiku",
                30,
                1.25,
                Some(20.0),
                true,
            ),
        ];
        let p = provider(
            "a",
            "anthropic",
            "https://api.anthropic.com",
            &[
                "claude-3-haiku-20240307",
                "claude-haiku-5-20260601",
                "claude-opus-5-20260801",
                "claude-sonnet-5-20260801",
            ],
        );
        assert_eq!(
            picked(&p, &catalog),
            Some("claude-sonnet-5-20260801".into())
        );
    }

    #[test]
    fn with_two_tiers_the_upper_is_picked() {
        let catalog = vec![
            entry(
                "openai/gpt-9-mini",
                "OpenAI: GPT-9 mini",
                1,
                2.0,
                Some(55.0),
                true,
            ),
            entry("openai/gpt-9", "OpenAI: GPT-9", 1, 10.0, Some(70.0), true),
        ];
        let p = provider(
            "o",
            "openai",
            "https://api.openai.com/v1",
            &["gpt-9", "gpt-9-mini", "whisper-1", "tts-1"],
        );
        assert_eq!(picked(&p, &catalog), Some("gpt-9".into()));
    }

    #[test]
    fn openrouter_picks_by_intelligence_under_the_price_cap() {
        let catalog = vec![
            // Too expensive.
            entry("x/frontier", "X: Frontier", 1, 30.0, Some(80.0), true),
            // Best under $5.
            entry("y/strong", "Y: Strong", 1, 4.0, Some(68.0), false),
            entry("z/small", "Z: Small", 2, 0.6, Some(55.0), false),
            // Smart and cheap, but six months old.
            entry("w/old-cheap", "W: Old Cheap", 6, 0.5, Some(66.0), true),
            // Free copy of the frontier model.
            entry(
                "x/frontier:free",
                "X: Frontier (free)",
                1,
                0.0,
                Some(80.0),
                true,
            ),
        ];
        let ids: Vec<&str> = catalog.iter().map(|e| e.id.as_str()).collect();
        let p = provider("r", "openrouter", "https://openrouter.ai/api/v1", &ids);
        assert_eq!(picked(&p, &catalog), Some("y/strong".into()));
    }

    #[test]
    fn the_window_widens_when_nothing_recent_qualifies() {
        let catalog = vec![
            entry("a/old", "A: Old", 8, 2.0, Some(60.0), true),
            entry("b/older", "B: Older", 20, 0.5, Some(70.0), true),
        ];
        let p = provider("r", "openrouter", "x", &["a/old", "b/older"]);
        // a/old is in the one-year window; b/older only once dates stop
        // counting.
        assert_eq!(picked(&p, &catalog), Some("a/old".into()));
    }

    #[test]
    fn unchecked_models_are_never_picked_unless_asked() {
        let catalog = vec![
            entry("y/strong", "Y: Strong", 1, 4.0, Some(68.0), true),
            entry("y/weaker", "Y: Weaker", 1, 0.5, Some(50.0), true),
        ];
        let mut p = provider("r", "openrouter", "x", &["y/strong", "y/weaker"]);
        p.enabled_models = vec!["y/weaker".into()];
        assert_eq!(picked(&p, &catalog), Some("y/weaker".into()));
        let models = provider_models(&p, &catalog, NOW);
        assert_eq!(
            pick(&p.provider_type, &p.base_url, &models, false, NOW),
            Some("y/strong".into())
        );
    }

    #[test]
    fn a_self_hosted_server_falls_back_to_its_first_model() {
        let p = provider(
            "l",
            "custom_openai",
            "http://box:8080",
            &["local-a", "local-b"],
        );
        assert_eq!(picked(&p, &[]), Some("local-a".into()));
    }
}
