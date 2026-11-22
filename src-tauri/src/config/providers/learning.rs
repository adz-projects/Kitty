//! The memory learning model: which model the memory engines learn with
//! (adaptive-pathway's extraction and cross-session synthesis, memorabilia's
//! extraction). Relayed per app to the engine (`/api/apps/me/learning-model`,
//! `lifecycle::overnight::apply_learning_model`).
//!
//! A chosen provider model may be any model a provider lists, checked or not,
//! like a Debate panel seat. It is synced to the engine as a provider row of
//! its own (its *card*, id [`LEARNING_CARD_ID`]), never offered to chats or
//! specialists and last for failover.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::debate::DebateSeat;
use super::ProviderProfile;
use crate::config::Config;

/// The card id of a chosen learning model.
pub const LEARNING_CARD_ID: &str = "learning-model";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LearningModelSetting {
    /// The local helper model, else the chat's own model.
    #[default]
    Auto,
    /// The chat's own model first (keeps this computer's CPU and GPU free);
    /// the helper model only if that fails.
    Chat,
    /// A model one of the user's providers lists.
    Provider { provider_id: String, model: String },
}

impl Config {
    /// The chosen learning model's card, when the choice is a provider model
    /// whose provider still exists (usable or not).
    pub fn learning_card(&self) -> Option<ProviderProfile> {
        let LearningModelSetting::Provider { provider_id, model } = &self.memory_learning_model
        else {
            return None;
        };
        let p = self.providers.iter().find(|p| &p.id == provider_id)?;
        let mut card = DebateSeat {
            id: LEARNING_CARD_ID.into(),
            name: "Memory learning".into(),
            provider_id: provider_id.clone(),
            model: model.clone(),
        }
        .card(p);
        card.id = LEARNING_CARD_ID.into();
        Some(card)
    }

    /// The document the engine should hold for this app. A provider choice
    /// whose card cannot be used (provider gone, model no longer listed) is
    /// relayed as `auto`, so learning never points at a row that is missing.
    pub fn learning_model_desired(&self) -> Value {
        match &self.memory_learning_model {
            LearningModelSetting::Auto => json!({"kind": "auto"}),
            LearningModelSetting::Chat => json!({"kind": "chat"}),
            LearningModelSetting::Provider { model, .. } => match self.learning_card() {
                Some(card) if card.is_usable() => json!({
                    "kind": "provider",
                    "provider_id": LEARNING_CARD_ID,
                    "model": model,
                }),
                _ => json!({"kind": "auto"}),
            },
        }
    }
}

/// Check a choice against `cfg`: a provider choice must name a provider that
/// lists the model.
pub fn validate(cfg: &Config, choice: &LearningModelSetting) -> Result<(), String> {
    if let LearningModelSetting::Provider { provider_id, model } = choice {
        let p = cfg
            .providers
            .iter()
            .find(|p| &p.id == provider_id)
            .ok_or_else(|| "That provider no longer exists.".to_string())?;
        if !p.models.iter().any(|m| m == model) {
            return Err(format!("{} does not offer {}.", p.name, model));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str, models: &[&str]) -> ProviderProfile {
        ProviderProfile {
            id: id.into(),
            name: format!("{id} name"),
            provider_type: "openrouter".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            models: models.iter().map(|m| m.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_provider_choice_becomes_a_card_and_is_relayed() {
        let mut cfg = Config {
            providers: vec![provider("or", &["big", "small"])],
            ..Default::default()
        };
        assert!(cfg.learning_card().is_none());
        assert_eq!(cfg.learning_model_desired(), json!({"kind": "auto"}));

        cfg.memory_learning_model = LearningModelSetting::Provider {
            provider_id: "or".into(),
            model: "big".into(),
        };
        let card = cfg.learning_card().unwrap();
        assert_eq!(card.id, LEARNING_CARD_ID);
        assert_eq!(card.models, ["big"]);
        assert_eq!(card.subagent_role.as_deref(), Some("never"));
        assert_eq!(
            cfg.learning_model_desired(),
            json!({"kind": "provider", "provider_id": LEARNING_CARD_ID, "model": "big"})
        );

        // A model the provider stopped listing falls back to auto.
        cfg.memory_learning_model = LearningModelSetting::Provider {
            provider_id: "or".into(),
            model: "gone".into(),
        };
        assert_eq!(cfg.learning_model_desired(), json!({"kind": "auto"}));
        assert!(validate(&cfg, &cfg.memory_learning_model.clone()).is_err());
    }

    #[test]
    fn wire_shape() {
        let v: LearningModelSetting = serde_json::from_value(json!({"kind": "chat"})).unwrap();
        assert_eq!(v, LearningModelSetting::Chat);
    }
}
