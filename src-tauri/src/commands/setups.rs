//! Setup commands (Settings → Setups). A setup is one of a provider's
//! checked models with its own name, thinking level, context length, system
//! prompt and specialist role (`config::providers::setups`). Chats, scheduled
//! tasks and specialists pick setups; `list_providers` lists them as cards.
//! Every change is mirrored to the engine (one provider row per setup).
//!
//! An edit reaches new chats only where it would otherwise change a running
//! chat's prompt: a chat keeps the system prompt and model it was started
//! with (both are pinned on the engine session), so its prompt cache holds.

use tauri::{AppHandle, Manager};

use crate::commands::ThinkingEffort;
use crate::config;
use crate::config::providers::setups::{name_from_model, new_setup_id};
use crate::config::providers::Setup;
use crate::state::AppState;

/// Create or update a setup. Returns it as saved (with its id).
#[tauri::command]
pub async fn upsert_setup(app: AppHandle, mut setup: Setup) -> Result<Setup, String> {
    setup.name = setup.name.trim().to_string();
    setup.system_prompt = setup.system_prompt.filter(|p| !p.trim().is_empty());
    setup.effort = setup.effort.filter(|e| !e.is_empty());
    // Read before the config lock: never hold both.
    let catalog = crate::ai_models::cached_catalog(&app);
    let (saved, card_id) = {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        let provider = cfg
            .providers
            .iter()
            .find(|p| p.id == setup.provider_id)
            .ok_or("Choose a provider for this setup.")?;
        if !provider.is_enabled(&setup.model) {
            return Err(format!(
                "Check {} in {}'s model list first.",
                setup.model, provider.name
            ));
        }
        if setup.name.is_empty() {
            setup.name = crate::ai_models::describe(provider, &setup.model, &catalog).name;
            if setup.name.is_empty() {
                setup.name = name_from_model(&setup.model);
            }
        }
        match cfg.setups.iter().position(|s| s.id == setup.id) {
            Some(i) => {
                setup.created_at = cfg.setups[i].created_at.clone();
                cfg.setups[i] = setup.clone();
            }
            None => {
                setup.id = new_setup_id(&cfg);
                setup.created_at = chrono::Utc::now().to_rfc3339();
                cfg.setups.push(setup.clone());
            }
        }
        if cfg.active_provider_id.is_none() {
            cfg.active_provider_id = Some(setup.id.clone());
            cfg.needs_default_provider = false;
        }
        cfg.repair_default(None);
        config::save(&cfg).map_err(|e| e.to_string())?;
        (setup.clone(), setup.id)
    };
    crate::bigtiny::effort::ensure_caps(&app, &card_id).await;
    if let Err(e) = crate::bigtiny::providers::sync_all_providers(&app).await {
        tracing::warn!("setup {card_id} saved but failed to sync to the engine: {e}");
    }
    crate::bigtiny::vision::ensure_vision_cached(&app, &card_id).await;
    Ok(saved)
}

/// Delete a setup. Its chats stay readable but cannot continue on it. When
/// it was the default, the next usable setup takes over; returns the
/// default's id.
#[tauri::command]
pub async fn delete_setup(app: AppHandle, id: String) -> Result<Option<String>, String> {
    let default = {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        cfg.setups.retain(|s| s.id != id);
        cfg.repair_default(None);
        config::save(&cfg).map_err(|e| e.to_string())?;
        cfg.active_provider_id.clone()
    };
    if let Err(e) = crate::bigtiny::providers::sync_all_providers(&app).await {
        tracing::warn!("deleted setup {id} but failed to sync the engine: {e}");
    }
    Ok(default)
}

/// Put the setups in this order (the order every model menu lists them
/// in). Ids not given keep their place after the ones given.
#[tauri::command]
pub async fn reorder_setups(app: AppHandle, ids: Vec<String>) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut cfg = state.config.lock().unwrap();
    let rank = |id: &str| ids.iter().position(|i| i == id).unwrap_or(usize::MAX);
    cfg.setups.sort_by_key(|s| rank(&s.id));
    config::save(&cfg).map_err(|e| e.to_string())
}

/// The thinking levels a provider's model offers, for the setup editor
/// (detected now if they never were). `None`: no control, or unknown.
#[tauri::command]
pub async fn get_model_effort(
    app: AppHandle,
    provider_id: String,
    model: String,
) -> Result<Option<ThinkingEffort>, String> {
    Ok(crate::bigtiny::effort::model_effort(&app, &provider_id, &model).await)
}
