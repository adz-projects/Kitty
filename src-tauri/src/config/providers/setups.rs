//! Setups: what a chat, a scheduled task or a specialist runs on. A setup is
//! one of a provider's checked models with the user's own name for it, a
//! default thinking level, an optional context length, an optional system
//! prompt and whether specialists may run on it. Several setups may share a
//! model ("Fast agent" and "Thought partner" on the same Opus).
//!
//! Everything below Settings sees a setup as its **card**: a
//! [`ProviderProfile`] with one model, the setup's id and name, its
//! provider's connection and the setup's own settings
//! ([`Config::card`], [`Config::cards`]). Each card is one engine provider
//! row, so a chat pinned to a setup is pinned to that row.
//!
//! Sampling settings (temperature and the rest) live on the setup too, as
//! they are about the model, but Kitty shows no controls for them: an
//! install that set them on a provider keeps them on that provider's setups.

use serde::{Deserialize, Serialize};

use super::ProviderProfile;
use crate::config::Config;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Setup {
    pub id: String,
    pub name: String,
    /// The provider whose model this is.
    pub provider_id: String,
    /// One of that provider's models. A model the provider no longer lists,
    /// or that was unchecked, leaves the setup unusable until it is edited.
    pub model: String,
    /// The thinking level a chat on this setup starts at, when the model
    /// offers it; `None` = the model's own default. A chat changes it turn by
    /// turn (`Config::session_efforts`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The user's context window for this setup; `None` = the window Kitty
    /// detected for the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u32>,
    /// `None` = Kitty's built-in prompt (`providers::DEFAULT_SYSTEM_PROMPT`).
    /// Set on a chat when it is created, so editing it changes new chats only
    /// and never a running chat's prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Whether specialists may run on this setup: `"preferred"`, `"allowed"`
    /// (the default when absent) or `"never"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub created_at: String,
}

/// Why a setup's card cannot be used: its model is no longer a checked model
/// of its provider.
pub const MODEL_UNAVAILABLE: &str = "model_unavailable";

impl Setup {
    /// This setup as its card on `provider`.
    pub fn card(&self, provider: &ProviderProfile) -> ProviderProfile {
        let model = self.model.clone();
        let mut card = provider.clone();
        card.provider_id = Some(provider.id.clone());
        card.id = self.id.clone();
        card.name = self.name.clone();
        card.models = vec![model.clone()];
        card.enabled_models = vec![model.clone()];
        card.model_ids = Default::default();
        card.supports_vision = provider.vision_models.contains(&model);
        card.vision_models = Vec::new();
        card.context_length = self
            .context_length
            .or_else(|| provider.context_lengths.get(&model).copied());
        card.context_lengths = Default::default();
        card.system_prompt = self.system_prompt.clone();
        card.subagent_role = self.subagent_role.clone();
        card.temperature = self.temperature;
        card.top_p = self.top_p;
        card.top_k = self.top_k;
        card.min_p = self.min_p;
        card.presence_penalty = self.presence_penalty;
        card.frequency_penalty = self.frequency_penalty;
        card.max_tokens = self.max_tokens;
        if card.disabled_reason.is_none() && !provider.is_enabled(&model) {
            card.disabled_reason = Some(MODEL_UNAVAILABLE.to_string());
        }
        card
    }
}

/// A setup's name when nothing better is known: the model id without its
/// vendor prefix (`"anthropic/claude-opus-4.5"` → `"claude-opus-4.5"`).
pub fn name_from_model(model: &str) -> String {
    model.rsplit('/').next().unwrap_or(model).to_string()
}

/// A setup id nothing else uses.
pub fn new_setup_id(cfg: &Config) -> String {
    let base = format!("setup_{}", chrono::Utc::now().timestamp_millis());
    let taken = |id: &str| {
        cfg.setups.iter().any(|s| s.id == id) || cfg.providers.iter().any(|p| p.id == id)
    };
    if !taken(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| !taken(id))
        .expect("an unused id")
}

impl Config {
    pub fn setup(&self, id: &str) -> Option<&Setup> {
        self.setups.iter().find(|s| s.id == id)
    }
}

/// Turn every checked model of every provider into a setup with its card's
/// id, so chats, scheduled tasks and specialists pinned to the card stay
/// where they were. Each takes the settings that were the provider's (its
/// prompt, context override, specialist role and sampling) and the thinking
/// level last used on the model; the provider keeps none of them. The
/// detected reasoning ladders move from the card's key to the provider's,
/// since they describe the model. Pure; idempotent.
pub fn migrate_to_setups(cfg: &mut Config) {
    if cfg.setups_migrated {
        return;
    }
    let created_at = chrono::Utc::now().to_rfc3339();
    let mut setups = Vec::new();
    for p in cfg.providers.iter_mut() {
        for model in p.checked_models().cloned().collect::<Vec<_>>() {
            let Some(id) = p.model_ids.get(&model).cloned() else {
                continue;
            };
            let old_key = crate::bigtiny::effort::effort_key(&id, &model);
            let new_key = crate::bigtiny::effort::effort_key(&p.id, &model);
            if let Some(caps) = cfg.effort_caps.get(&old_key).cloned() {
                cfg.effort_caps.entry(new_key).or_insert(caps);
            }
            setups.push(Setup {
                id,
                name: name_from_model(&model),
                provider_id: p.id.clone(),
                effort: cfg.model_efforts.get(&old_key).cloned(),
                context_length: p.context_length,
                system_prompt: p.system_prompt.clone().filter(|s| !s.trim().is_empty()),
                subagent_role: p.subagent_role.clone(),
                temperature: p.temperature,
                top_p: p.top_p,
                top_k: p.top_k,
                min_p: p.min_p,
                presence_penalty: p.presence_penalty,
                frequency_penalty: p.frequency_penalty,
                max_tokens: p.max_tokens,
                created_at: created_at.clone(),
                model,
            });
        }
        p.context_length = None;
        p.system_prompt = None;
        p.subagent_role = None;
        p.temperature = None;
        p.top_p = None;
        p.top_k = None;
        p.min_p = None;
        p.presence_penalty = None;
        p.frequency_penalty = None;
        p.max_tokens = None;
    }
    cfg.setups.extend(setups);
    cfg.model_efforts.clear();
    cfg.setups_migrated = true;
}

/// Point every setup at the provider that now holds its model, after
/// providers of one service were merged (`merge_same_service` keeps each
/// model's card id, which is the setup's id). Returns whether anything
/// changed. Pure.
pub fn repoint_setups(cfg: &mut Config) -> bool {
    let mut changed = false;
    for s in cfg.setups.iter_mut() {
        if cfg.providers.iter().any(|p| p.id == s.provider_id) {
            continue;
        }
        if let Some(p) = cfg
            .providers
            .iter()
            .find(|p| p.model_ids.values().any(|id| *id == s.id))
        {
            s.provider_id = p.id.clone();
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::providers::upgrade_to_model_lists;

    fn provider(id: &str, models: &[&str]) -> ProviderProfile {
        let mut p: ProviderProfile = serde_json::from_value(serde_json::json!({
            "id": id, "name": id, "provider_type": "custom_openai",
            "base_url": "http://box:8080", "models": models,
        }))
        .unwrap();
        upgrade_to_model_lists(std::slice::from_mut(&mut p));
        p
    }

    #[test]
    fn every_checked_model_becomes_a_setup_with_its_card_id() {
        let mut cfg = Config {
            setups_migrated: false,
            ..Default::default()
        };
        let mut p = provider("p", &["x/a", "b"]);
        p.system_prompt = Some("Be terse.".into());
        p.context_length = Some(32_000);
        p.temperature = Some(0.2);
        p.subagent_role = Some("never".into());
        cfg.providers = vec![p];
        cfg.model_efforts.insert(
            crate::bigtiny::effort::effort_key("p-m1", "b"),
            "high".into(),
        );
        migrate_to_setups(&mut cfg);

        let ids: Vec<&str> = cfg.setups.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["p", "p-m1"]);
        let a = &cfg.setups[0];
        assert_eq!(a.name, "a");
        assert_eq!(a.model, "x/a");
        assert_eq!(a.system_prompt.as_deref(), Some("Be terse."));
        assert_eq!(a.context_length, Some(32_000));
        assert_eq!(a.temperature, Some(0.2));
        assert_eq!(a.subagent_role.as_deref(), Some("never"));
        assert_eq!(cfg.setups[1].effort.as_deref(), Some("high"));

        let p = &cfg.providers[0];
        assert!(p.system_prompt.is_none() && p.context_length.is_none());
        assert!(p.temperature.is_none() && p.subagent_role.is_none());

        // The card a chat was pinned to is now the setup of the same id.
        let card = cfg.card("p-m1").unwrap();
        assert_eq!(card.model(), "b");
        assert_eq!(card.secret_account(), "p");
        assert_eq!(card.system_prompt.as_deref(), Some("Be terse."));
        assert!(card.is_usable());

        // Once only.
        let before = cfg.setups.clone();
        migrate_to_setups(&mut cfg);
        assert_eq!(cfg.setups, before);
    }

    #[test]
    fn reasoning_ladders_move_to_the_provider_key() {
        let mut cfg = Config {
            setups_migrated: false,
            ..Default::default()
        };
        cfg.providers = vec![provider("p", &["a", "b"])];
        let caps: crate::bigtiny::effort::EffortCaps = serde_json::from_value(serde_json::json!({
            "levels": ["low", "high"], "can_disable": false, "source": "open_router",
            "detected_at": 1
        }))
        .unwrap();
        cfg.effort_caps
            .insert(crate::bigtiny::effort::effort_key("p-m1", "b"), caps);
        migrate_to_setups(&mut cfg);
        assert!(cfg
            .effort_caps
            .contains_key(&crate::bigtiny::effort::effort_key("p", "b")));
    }

    #[test]
    fn an_unchecked_model_leaves_its_setup_unusable() {
        let mut cfg = Config {
            setups_migrated: false,
            ..Default::default()
        };
        cfg.providers = vec![provider("p", &["a", "b"])];
        migrate_to_setups(&mut cfg);
        cfg.providers[0].enabled_models = vec!["a".into()];
        let card = cfg.card("p-m1").unwrap();
        assert_eq!(card.disabled_reason.as_deref(), Some(MODEL_UNAVAILABLE));
        assert!(!card.is_usable());
        assert_eq!(cfg.cards().len(), 2, "still listed, so it can be edited");
    }

    #[test]
    fn a_setups_settings_win_over_what_was_detected() {
        let mut cfg = Config {
            setups_migrated: false,
            ..Default::default()
        };
        let mut p = provider("p", &["a"]);
        p.context_lengths.insert("a".into(), 128_000);
        p.vision_models = vec!["a".into()];
        cfg.providers = vec![p];
        migrate_to_setups(&mut cfg);
        assert_eq!(cfg.card("p").unwrap().context_length, Some(128_000));
        assert!(cfg.card("p").unwrap().supports_vision);
        cfg.setups[0].context_length = Some(16_000);
        assert_eq!(cfg.card("p").unwrap().context_length, Some(16_000));
    }

    #[test]
    fn setups_follow_their_model_into_a_merged_provider() {
        let mut cfg = Config {
            setups_migrated: false,
            ..Default::default()
        };
        cfg.providers = vec![provider("a", &["x"]), provider("b", &["y"])];
        migrate_to_setups(&mut cfg);
        let mut providers = cfg.providers.clone();
        crate::config::providers::merge_same_service(&mut providers, &[None, None]);
        cfg.providers = providers;
        assert!(repoint_setups(&mut cfg));
        assert_eq!(cfg.setup("b").unwrap().provider_id, "a");
        assert_eq!(cfg.card("b").unwrap().model(), "y");
    }
}
