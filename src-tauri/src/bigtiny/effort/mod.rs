//! Reasoning effort: which levels each card's model offers, which one a chat
//! is on, and keeping the engine in step with both.
//!
//! * [`ladder`] — the pure rules: level order, the default, stars.
//! * [`detect`] — finding a card's ladder (provider API, OpenRouter catalog,
//!   chat templates, name patterns as the last resort).
//! * this module — resolving a **chat's** effort against **that chat's own
//!   card**, persisting detections, and the triggers that run them.
//!
//! Every chat is pinned to a card in the engine (`metadata.provider`), and
//! that card — not the global default — is what the engine sends the turn
//! on. Effort used to be resolved against the default card, so a chat on any
//! other card showed (and confirmed, and remembered) the wrong ladder;
//! [`resolve_session_card`] is the fix, and everything here goes through it.
//!
//! Resolution order for a chat's level, each candidate checked against the
//! levels its card actually offers:
//!   1. the chat's own choice (`Config::session_efforts`);
//!   2. the level last used on this card+model (`Config::model_efforts`);
//!   3. the ladder default (`medium` if offered, else the middle).

pub mod detect;
pub mod ladder;

use std::collections::HashSet;

use serde::Serialize;
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};

use crate::commands::{EffortOption, ThinkingEffort};
use crate::config::Config;
use crate::state::AppState;
use detect::{CardSnapshot, Detection};
pub use ladder::{EffortCaps, EffortSource};

/// A detection older than this is refreshed in the background the next time
/// the card is used. Model ladders change on the order of months.
const STALE_AFTER_SECS: i64 = 30 * 24 * 60 * 60;

/// Key for the per-card+model maps (`effort_caps`, `model_efforts`) — the
/// same `provider\0model` shape `context_window` and `vision` use.
pub fn effort_key(card_id: &str, model: &str) -> String {
    format!("{card_id}\u{0}{model}")
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

fn card_model(cfg: &Config, card_id: &str) -> Option<String> {
    let p = cfg.providers.iter().find(|p| p.id == card_id)?;
    Some(p.models.first().cloned().unwrap_or_default())
}

fn snapshot(cfg: &Config, card_id: &str) -> Option<CardSnapshot> {
    let p = cfg.providers.iter().find(|p| p.id == card_id)?;
    Some(CardSnapshot {
        id: p.id.clone(),
        provider_type: p.provider_type.clone(),
        base_url: p.base_url.clone(),
        model: p.models.first().cloned().unwrap_or_default(),
    })
}

// ------------------------------------------------------------ chat → card

/// Record which card (and model) a chat is on. Called wherever Kitty learns
/// it: creating, loading or branching a chat, and moving it to a card.
pub fn remember_session_card(app: &AppHandle, session_id: &str, card_id: &str, model: &str) {
    if session_id.is_empty() || card_id.is_empty() {
        return;
    }
    let state = app.state::<AppState>();
    state
        .session_cards
        .lock()
        .unwrap()
        .insert(session_id.to_string(), (card_id.to_string(), model.to_string()));
}

/// The chat's card if already known this run, else the default card. Sync,
/// for the callers that build a payload without awaiting.
fn known_or_default_card(app: &AppHandle, session_id: &str) -> Option<(String, String)> {
    let state = app.state::<AppState>();
    if let Some(c) = state.session_cards.lock().unwrap().get(session_id).cloned() {
        return Some(c);
    }
    let cfg = state.config.lock().unwrap();
    let id = cfg.active_provider_id.clone()?;
    let model = card_model(&cfg, &id)?;
    Some((id, model))
}

/// The card a chat runs on: remembered this run, else read from the engine's
/// session metadata (and remembered), else the default card — the one a chat
/// that has no pin yet would be created on.
pub async fn resolve_session_card(app: &AppHandle, session_id: &str) -> Option<(String, String)> {
    let known = {
        let state = app.state::<AppState>();
        let c = state.session_cards.lock().unwrap().get(session_id).cloned();
        c
    };
    if known.is_some() {
        return known;
    }
    if !session_id.is_empty() {
        if let Ok(client) = crate::bigtiny::client::ensure_client(app) {
            let meta = crate::bigtiny::sessions::metadata(&client, session_id).await;
            let text = |k: &str| {
                meta.get(k)
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
            };
            if let Some(card) = text("provider") {
                let model = match text("model") {
                    Some(m) => m,
                    None => {
                        let state = app.state::<AppState>();
                        let cfg = state.config.lock().unwrap();
                        card_model(&cfg, &card).unwrap_or_default()
                    }
                };
                remember_session_card(app, session_id, &card, &model);
                return Some((card, model));
            }
        }
    }
    known_or_default_card(app, session_id)
}

// ------------------------------------------------------------- resolution

/// The picker payload for a chat on `card_id`/`model`, from a config
/// snapshot. `None` when the card's model has no control or hasn't been
/// detected yet — the picker is then hidden.
pub fn effort_view(
    cfg: &Config,
    card_id: &str,
    model: &str,
    session_id: Option<&str>,
) -> Option<ThinkingEffort> {
    let key = effort_key(card_id, model);
    let caps = cfg.effort_caps.get(&key)?;
    if !caps.has_control() {
        return None;
    }
    let opts = ladder::options(caps);
    let offered = |v: &String| opts.iter().any(|o| &o.value == v);
    let fallback = ladder::default_level(&caps.levels)
        .or_else(|| opts.first().map(|o| o.value.clone()))?;
    let current_value = session_id
        .and_then(|sid| cfg.session_efforts.get(sid))
        .filter(|v| offered(v))
        .or_else(|| cfg.model_efforts.get(&key).filter(|v| offered(v)))
        .cloned()
        .unwrap_or(fallback);
    Some(ThinkingEffort {
        current_value,
        max_stars: u8::try_from(caps.levels.len()).unwrap_or(u8::MAX),
        can_disable: caps.can_disable,
        options: opts
            .into_iter()
            .map(|o| EffortOption {
                name: o.name,
                value: o.value,
                stars: o.stars,
            })
            .collect(),
    })
}

/// Sync read for payload builders that can't await (`session_info`, the
/// window adopt payload). Uses the chat's card when it's already known this
/// run — `session_info` records it first — else the default card.
pub fn thinking_effort_for(app: &AppHandle, session_id: &str) -> Option<ThinkingEffort> {
    let (card, model) = known_or_default_card(app, session_id)?;
    let state = app.state::<AppState>();
    let cfg = state.config.lock().unwrap();
    effort_view(&cfg, &card, &model, Some(session_id))
}

/// The full read: resolve the chat's card (asking the engine if needed),
/// make sure its ladder is detected, then resolve the level.
pub async fn thinking_effort_for_session(
    app: &AppHandle,
    session_id: &str,
) -> Option<ThinkingEffort> {
    let (card, model) = resolve_session_card(app, session_id).await?;
    ensure_caps(app, &card).await;
    let state = app.state::<AppState>();
    let cfg = state.config.lock().unwrap();
    effort_view(&cfg, &card, &model, Some(session_id))
}

/// Make sure a card has a ladder: detect now (awaited) when it has none, or
/// in the background when the one it has is stale.
pub async fn ensure_caps(app: &AppHandle, card_id: &str) {
    let (missing, stale) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        let Some(model) = card_model(&cfg, card_id) else {
            return;
        };
        match cfg.effort_caps.get(&effort_key(card_id, &model)) {
            None => (true, false),
            Some(c) => (false, c.stale || now_unix() - c.detected_at > STALE_AFTER_SECS),
        }
    };
    if missing {
        detect_card(app, card_id).await;
    } else if stale {
        spawn_detect(app, card_id);
    }
}

// -------------------------------------------------------------- detection

/// Detect one card's ladder and store it. Returns whether anything changed.
///
/// A detection that learns nothing (`Unknown` — endpoint down, nothing
/// matched) leaves the stored ladder alone, so a failed re-detect never
/// leaves a card worse off. Deduplicated per card: a second call while one
/// is running returns at once.
pub async fn detect_card(app: &AppHandle, card_id: &str) -> bool {
    let card = {
        let state = app.state::<AppState>();
        if !state.effort_detecting.lock().unwrap().insert(card_id.to_string()) {
            return false;
        }
        let cfg = state.config.lock().unwrap();
        snapshot(&cfg, card_id)
    };
    let changed = match card {
        Some(card) => run_detection(app, &card).await,
        None => false,
    };
    app.state::<AppState>()
        .effort_detecting
        .lock()
        .unwrap()
        .remove(card_id);
    let _ = app.emit(
        "provider://effort-detected",
        json!({ "provider_id": card_id, "changed": changed }),
    );
    changed
}

async fn run_detection(app: &AppHandle, card: &CardSnapshot) -> bool {
    let catalog = {
        let state = app.state::<AppState>();
        crate::openrouter::catalog::ensure_catalog_fresh(&state).await;
        let guard = state.openrouter_catalog.lock().unwrap();
        guard.as_ref().map(|c| c.entries.clone()).unwrap_or_default()
    };
    let api_key = crate::config::providers::get_secret_async(&card.id).await;
    let found = match detect::detect(card, &catalog, api_key.as_deref()).await {
        Detection::Found(c) => c,
        Detection::Unknown => {
            tracing::info!(card = card.id, model = card.model, "reasoning levels not detected");
            let state = app.state::<AppState>();
            let mut cfg = state.config.lock().unwrap();
            // Keep what we had, but stop treating a rejected ladder as stale
            // on every read — the next scheduled refresh tries again.
            if let Some(c) = cfg.effort_caps.get_mut(&effort_key(&card.id, &card.model)) {
                if c.stale {
                    c.stale = false;
                    let _ = crate::config::save(&cfg);
                }
            }
            return false;
        }
    };
    tracing::info!(
        card = card.id,
        model = card.model,
        levels = ?found.levels,
        can_disable = found.can_disable,
        source = ?found.source,
        "detected reasoning levels"
    );
    let (changed, resync) = {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        let key = effort_key(&card.id, &card.model);
        let prev = cfg.effort_caps.get(&key).cloned();
        let changed = prev
            .as_ref()
            .map_or(true, |p| p.levels != found.levels || p.can_disable != found.can_disable);
        let resync = prev.as_ref().and_then(|p| p.anthropic_thinking.clone())
            != found.anthropic_thinking;
        cfg.effort_caps.insert(key, found);
        if let Err(e) = crate::config::save(&cfg) {
            tracing::warn!("failed to persist detected reasoning levels: {e}");
        }
        (changed, resync)
    };
    // The Anthropic wire mode rides the provider row, so the engine needs the
    // card re-synced when it changes.
    if resync {
        if let Err(e) = crate::bigtiny::providers::sync_all_providers(app).await {
            tracing::warn!("could not re-sync providers after effort detection: {e}");
        }
    }
    changed
}

/// [`detect_card`] in the background.
pub fn spawn_detect(app: &AppHandle, card_id: &str) {
    let app = app.clone();
    let card_id = card_id.to_string();
    tauri::async_runtime::spawn(async move {
        detect_card(&app, &card_id).await;
    });
}

/// Detect every card — or, with `only_missing`, every card that has no
/// ladder yet (startup, and after an upgrade from a Kitty that kept none).
/// Sequential: they mostly share one catalog, and a burst of `/props` probes
/// at once buys nothing.
pub async fn detect_all(app: &AppHandle, only_missing: bool) {
    let ids: Vec<String> = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        cfg.providers
            .iter()
            .filter(|p| p.is_usable())
            .filter(|p| {
                !only_missing || {
                    let model = p.models.first().cloned().unwrap_or_default();
                    !cfg.effort_caps.contains_key(&effort_key(&p.id, &model))
                }
            })
            .map(|p| p.id.clone())
            .collect()
    };
    for id in ids {
        detect_card(app, &id).await;
    }
}

/// The engine reported that a chat's model rejected its reasoning level.
/// Mark the card stale, re-detect in the background, and tell the chat when
/// its options changed.
pub fn on_effort_rejected(app: &AppHandle, session_id: &str) {
    let app = app.clone();
    let session_id = session_id.to_string();
    tauri::async_runtime::spawn(async move {
        let Some((card, model)) = resolve_session_card(&app, &session_id).await else {
            return;
        };
        let name = {
            let state = app.state::<AppState>();
            let mut cfg = state.config.lock().unwrap();
            if let Some(c) = cfg.effort_caps.get_mut(&effort_key(&card, &model)) {
                c.stale = true;
            }
            let _ = crate::config::save(&cfg);
            cfg.providers
                .iter()
                .find(|p| p.id == card)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| model.clone())
        };
        if detect_card(&app, &card).await {
            reapply_after_card_change(&app, &session_id).await;
            let _ = app.emit(
                "chat://notice",
                json!({
                    "session_id": session_id,
                    "kind": "effort_updated",
                    "message": format!("Reasoning options for {name} were updated."),
                }),
            );
        }
    });
}

// ---------------------------------------------------- keeping the engine in step

/// Materialize a chat's effective level on its **first turn**: persist it as
/// the chat's own choice, remember it for this card+model, and push it to the
/// engine so the wire matches the picker.
///
/// Until this runs, a chat that never touched the picker shows a default it
/// never sent. Doing it at first send (rather than at creation) lets the user
/// pick a level before sending and have *that* be the one remembered. Every
/// write is conditional on an actual change, and failures are logged, never
/// surfaced: the turn must still send.
pub async fn confirm_model_effort(app: &AppHandle, session_id: &str) {
    let Some((card, model)) = resolve_session_card(app, session_id).await else {
        return;
    };
    ensure_caps(app, &card).await;
    let push = {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        let Some(effort) = effort_view(&cfg, &card, &model, Some(session_id)) else {
            return; // no control on this card — nothing to confirm
        };
        let value = effort.current_value;
        let key = effort_key(&card, &model);
        let session_changed = cfg.session_efforts.get(session_id) != Some(&value);
        let model_changed = cfg.model_efforts.get(&key) != Some(&value);
        if session_changed {
            cfg.session_efforts.insert(session_id.to_string(), value.clone());
        }
        if model_changed {
            cfg.model_efforts.insert(key, value.clone());
        }
        if session_changed || model_changed {
            if let Err(e) = crate::config::save(&cfg) {
                tracing::warn!("failed to persist confirmed thinking effort: {e}");
            }
        }
        // A chat already carrying this value was PATCHed when it was set.
        session_changed.then_some(value)
    };
    if let Some(value) = push {
        if let Err(e) =
            crate::bigtiny::sessions::update_thinking_effort(app, session_id, &value).await
        {
            tracing::warn!("failed to push confirmed thinking effort to the engine: {e}");
        }
    }
}

/// A chat moved to another card (or its card's ladder changed): keep its
/// level if the new ladder offers it, otherwise fall to the remembered or
/// default level, and tell the engine at once rather than at the next
/// first-turn confirmation — a chat with history never has one.
pub async fn reapply_after_card_change(app: &AppHandle, session_id: &str) {
    let Some((card, model)) = resolve_session_card(app, session_id).await else {
        return;
    };
    ensure_caps(app, &card).await;
    let push = {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        let Some(stored) = cfg.session_efforts.get(session_id).cloned() else {
            // Never confirmed (an unsent chat): its first turn will.
            return;
        };
        let Some(effort) = effort_view(&cfg, &card, &model, Some(session_id)) else {
            return;
        };
        if effort.current_value == stored {
            None
        } else {
            cfg.session_efforts
                .insert(session_id.to_string(), effort.current_value.clone());
            let _ = crate::config::save(&cfg);
            Some(effort.current_value)
        }
    };
    if let Some(value) = push {
        if let Err(e) =
            crate::bigtiny::sessions::update_thinking_effort(app, session_id, &value).await
        {
            tracing::warn!("failed to push re-resolved thinking effort to the engine: {e}");
        }
    }
}

// ----------------------------------------------------------------- Settings

/// What Settings → Providers shows for one card.
#[derive(Debug, Clone, Serialize)]
pub struct EffortSummary {
    /// Ascending ladder, Off excluded.
    pub levels: Vec<String>,
    pub can_disable: bool,
    pub source: Option<EffortSource>,
    /// "OpenRouter", "the provider", … for the "via …" line.
    pub source_label: Option<String>,
    /// Unix seconds; `None` when never detected.
    pub detected_at: Option<i64>,
    pub detecting: bool,
}

pub fn summary_for_profile(
    cfg: &Config,
    profile: &crate::config::providers::ProviderProfile,
    detecting: &HashSet<String>,
) -> EffortSummary {
    let model = profile.models.first().cloned().unwrap_or_default();
    let caps = cfg.effort_caps.get(&effort_key(&profile.id, &model));
    EffortSummary {
        levels: caps.map(|c| c.levels.clone()).unwrap_or_default(),
        can_disable: caps.is_some_and(|c| c.can_disable),
        source: caps.map(|c| c.source),
        source_label: caps.map(|c| c.source.label().to_string()),
        detected_at: caps.map(|c| c.detected_at),
        detecting: detecting.contains(&profile.id),
    }
}

/// The Anthropic wire mode to put on a card's engine row, if detected.
pub fn anthropic_thinking_for(
    cfg: &Config,
    profile: &crate::config::providers::ProviderProfile,
) -> Option<String> {
    if profile.provider_type != "anthropic" {
        return None;
    }
    let model = profile.models.first()?;
    cfg.effort_caps
        .get(&effort_key(&profile.id, model))
        .and_then(|c| c.anthropic_thinking.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::providers::ProviderProfile;

    fn caps(levels: &[&str], can_disable: bool) -> EffortCaps {
        EffortCaps {
            levels: levels.iter().map(|s| s.to_string()).collect(),
            can_disable,
            anthropic_thinking: None,
            source: EffortSource::OpenRouter,
            detected_at: 0,
            stale: false,
        }
    }

    fn profile(id: &str, ty: &str, model: &str) -> ProviderProfile {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": id, "provider_type": ty, "base_url": "", "models": [model],
        }))
        .unwrap()
    }

    /// The bug this module exists to fix: two chats on two cards each get
    /// their own card's ladder, whatever the default is.
    #[test]
    fn each_chat_resolves_against_its_own_card() {
        let mut cfg = Config {
            providers: vec![
                profile("a", "openai", "gpt-4o"),
                profile("b", "openrouter", "anthropic/claude-haiku-5.5"),
            ],
            active_provider_id: Some("a".into()),
            ..Config::default()
        };
        cfg.effort_caps
            .insert(effort_key("a", "gpt-4o"), caps(&[], false));
        cfg.effort_caps.insert(
            effort_key("b", "anthropic/claude-haiku-5.5"),
            caps(&["low", "medium", "high", "xhigh", "max"], true),
        );
        // The default card has no control...
        assert!(effort_view(&cfg, "a", "gpt-4o", Some("s1")).is_none());
        // ...but a chat on card b still gets b's ladder, starting at medium.
        let e = effort_view(&cfg, "b", "anthropic/claude-haiku-5.5", Some("s2")).unwrap();
        assert_eq!(e.current_value, "medium");
        assert_eq!(e.options[0].value, "off");
        assert_eq!(e.max_stars, 5);
        assert!(e.can_disable);
    }

    #[test]
    fn chosen_then_remembered_then_default() {
        let mut cfg = Config::default();
        let key = effort_key("b", "m");
        cfg.effort_caps.insert(key.clone(), caps(&["low", "medium", "high"], false));
        assert_eq!(effort_view(&cfg, "b", "m", Some("s")).unwrap().current_value, "medium");
        cfg.model_efforts.insert(key.clone(), "high".into());
        assert_eq!(effort_view(&cfg, "b", "m", Some("s")).unwrap().current_value, "high");
        cfg.session_efforts.insert("s".into(), "low".into());
        assert_eq!(effort_view(&cfg, "b", "m", Some("s")).unwrap().current_value, "low");
        // A value this ladder doesn't offer (here Off on a mandatory model)
        // never sticks.
        cfg.session_efforts.insert("s".into(), "off".into());
        cfg.model_efforts.insert(key, "xhigh".into());
        assert_eq!(effort_view(&cfg, "b", "m", Some("s")).unwrap().current_value, "medium");
    }

    #[test]
    fn undetected_cards_show_no_picker() {
        let cfg = Config::default();
        assert!(effort_view(&cfg, "x", "y", None).is_none());
    }

    #[test]
    fn toggle_only_defaults_to_on() {
        let mut cfg = Config::default();
        cfg.effort_caps.insert(effort_key("c", "m"), caps(&["on"], true));
        let e = effort_view(&cfg, "c", "m", None).unwrap();
        assert_eq!(e.current_value, "on");
        assert_eq!(e.options.len(), 2);
    }
}
