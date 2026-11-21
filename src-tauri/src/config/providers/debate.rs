//! The Debate panel: the models a `/debate` may seat its personas on.
//!
//! A seat is any model one of the user's providers lists, checked or not and
//! whether or not a setup uses it, because a debate wants capable models and
//! setups are usually made for cheap ones. Each seat is synced to the engine
//! as a provider row of its own (its *card*, like a setup's), so a debater can
//! be pinned to it. A seat card is marked never to host ordinary specialists
//! and sorts last for failover: it is reachable only through a debate's panel.

use serde::{Deserialize, Serialize};

use super::setups::{Setup, MODEL_UNAVAILABLE};
use super::ProviderProfile;
use crate::config::Config;

/// Prefix of every seat's card id, so a seat can never collide with a setup.
pub const SEAT_ID_PREFIX: &str = "debate-";

/// Lowest priority Kitty gives a row, so ordinary routing reaches a seat only
/// when nothing else is left.
pub const SEAT_FALLBACK_PRIORITY: i32 = 100;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DebateSeat {
    pub id: String,
    /// The user's name for the seat; the moderator sees it.
    pub name: String,
    pub provider_id: String,
    pub model: String,
}

impl DebateSeat {
    /// This seat as its card on `provider`. Unusable when the provider no
    /// longer lists the model (being unchecked does not matter here).
    pub fn card(&self, provider: &ProviderProfile) -> ProviderProfile {
        let mut card = Setup {
            id: self.id.clone(),
            name: self.name.clone(),
            provider_id: provider.id.clone(),
            model: self.model.clone(),
            subagent_role: Some("never".into()),
            ..Default::default()
        }
        .card(provider);
        let listed = provider.models.iter().any(|m| m == &self.model);
        if card.disabled_reason.as_deref() == Some(MODEL_UNAVAILABLE)
            && listed
            && provider.disabled_reason.is_none()
        {
            card.disabled_reason = None;
        }
        if !listed && card.disabled_reason.is_none() {
            card.disabled_reason = Some(MODEL_UNAVAILABLE.to_string());
        }
        card
    }
}

/// Make `seats` a valid panel against `cfg`: drop seats whose provider is gone
/// or whose model it does not list, name unnamed ones after their model, and
/// give new seats (empty id) an id of their own.
pub fn normalize_panel(cfg: &Config, seats: Vec<DebateSeat>) -> Result<Vec<DebateSeat>, String> {
    let mut out: Vec<DebateSeat> = Vec::new();
    for (i, mut seat) in seats.into_iter().enumerate() {
        let Some(p) = cfg.providers.iter().find(|p| p.id == seat.provider_id) else {
            return Err("A seat's provider no longer exists; remove the seat.".into());
        };
        if !p.models.iter().any(|m| m == &seat.model) {
            return Err(format!("{} does not offer {}.", p.name, seat.model));
        }
        if seat.name.trim().is_empty() {
            seat.name = super::setups::name_from_model(&seat.model);
        }
        seat.name = seat.name.trim().to_string();
        if !is_seat_id(&seat.id) || out.iter().any(|s| s.id == seat.id) {
            let base = format!(
                "{SEAT_ID_PREFIX}{}-{i}",
                chrono::Utc::now().timestamp_millis()
            );
            seat.id = base;
        }
        out.push(seat);
    }
    Ok(out)
}

/// Whether a card id is a debate seat's.
pub fn is_seat_id(id: &str) -> bool {
    id.starts_with(SEAT_ID_PREFIX)
}

/// One host a debate may use, as the moderator is shown it.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelHost {
    /// The engine row id (`host` in `call_specialist`).
    pub id: String,
    pub name: String,
    pub provider_name: String,
    pub model: String,
}

impl Config {
    /// Every seat's card, usable or not, in panel order.
    pub fn seat_cards(&self) -> Vec<ProviderProfile> {
        self.specialists
            .debate_panel
            .iter()
            .filter_map(|s| {
                let p = self.providers.iter().find(|p| p.id == s.provider_id)?;
                Some(s.card(p))
            })
            .collect()
    }

    /// The hosts a `/debate` turn may seat personas on: the panel's usable
    /// seats, or — with no usable seat — every usable setup, whatever its
    /// specialist role. Fewer than two means there is no cross-model debate
    /// to have.
    pub fn debate_hosts(&self) -> Vec<PanelHost> {
        let provider_name = |card: &ProviderProfile| {
            card.provider_id
                .as_deref()
                .and_then(|pid| self.providers.iter().find(|p| p.id == pid))
                .map(|p| p.name.clone())
                .unwrap_or_default()
        };
        let to_host = |card: ProviderProfile| PanelHost {
            provider_name: provider_name(&card),
            model: card.models.first().cloned().unwrap_or_default(),
            id: card.id,
            name: card.name,
        };
        let seats: Vec<PanelHost> = self
            .seat_cards()
            .into_iter()
            .filter(|c| c.is_usable())
            .map(to_host)
            .collect();
        if !seats.is_empty() {
            return seats;
        }
        self.cards()
            .into_iter()
            .filter(|c| c.is_usable())
            .map(to_host)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str, models: &[&str], checked: &[&str]) -> ProviderProfile {
        ProviderProfile {
            id: id.into(),
            name: format!("{id} name"),
            provider_type: "openrouter".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            models: models.iter().map(|m| m.to_string()).collect(),
            enabled_models: checked.iter().map(|m| m.to_string()).collect(),
            ..Default::default()
        }
    }

    fn seat(id: &str, provider_id: &str, model: &str) -> DebateSeat {
        DebateSeat {
            id: format!("{SEAT_ID_PREFIX}{id}"),
            name: id.into(),
            provider_id: provider_id.into(),
            model: model.into(),
        }
    }

    #[test]
    fn a_seat_may_be_any_listed_model_even_unchecked() {
        let p = provider("or", &["big", "small"], &["small"]);
        let card = seat("a", "or", "big").card(&p);
        assert!(card.is_usable(), "{:?}", card.disabled_reason);
        assert_eq!(card.models, ["big"]);
        assert_eq!(card.subagent_role.as_deref(), Some("never"));

        let gone = seat("b", "or", "retired").card(&p);
        assert!(!gone.is_usable());
    }

    #[test]
    fn hosts_come_from_the_panel_then_the_setups() {
        let mut cfg = Config {
            providers: vec![provider("or", &["big", "small"], &["small"])],
            setups: vec![Setup {
                id: "s1".into(),
                name: "Daily".into(),
                provider_id: "or".into(),
                model: "small".into(),
                subagent_role: Some("never".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        // No panel: the setups, whatever their specialist role.
        let hosts = cfg.debate_hosts();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].id, "s1");
        assert_eq!(hosts[0].provider_name, "or name");

        cfg.specialists.debate_panel = vec![seat("a", "or", "big"), seat("b", "or", "small")];
        let ids: Vec<_> = cfg.debate_hosts().into_iter().map(|h| h.id).collect();
        assert_eq!(ids, ["debate-a", "debate-b"]);

        // A panel with nothing usable falls back to the setups.
        cfg.specialists.debate_panel = vec![seat("c", "or", "retired")];
        assert_eq!(cfg.debate_hosts()[0].id, "s1");
    }
}
