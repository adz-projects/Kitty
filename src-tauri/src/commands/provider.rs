//! Provider profile commands: list/create/update/delete + activation (which
//! restarts goosed and warms/evicts local Ollama models around the switch).

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::config;
use crate::config::providers::{self, NetworkTier, ProviderProfile};
use crate::config::Config;
use crate::ollama;
use crate::state::AppState;

use super::window::restart_goosed;

/// A provider profile plus derived fields the UI needs.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderView {
    #[serde(flatten)]
    pub profile: ProviderProfile,
    pub network_tier: NetworkTier,
    pub has_secret: bool,
    pub active: bool,
}

fn provider_views(cfg: &Config) -> Vec<ProviderView> {
    cfg.providers
        .iter()
        .map(|p| ProviderView {
            network_tier: p.network_tier(),
            has_secret: providers::has_secret(&p.id),
            active: cfg.active_provider_id.as_deref() == Some(&p.id),
            profile: p.clone(),
        })
        .collect()
}

/// List provider profiles with derived tier / secret / active flags.
#[tauri::command]
pub fn list_providers(state: tauri::State<'_, AppState>) -> Result<Vec<ProviderView>, String> {
    let cfg = state.config.lock().unwrap();
    Ok(provider_views(&cfg))
}

/// Create or update a provider profile. `secret`, when present, is stored in the
/// keyring only (never in config.json). Returns the saved profile (with id).
#[tauri::command]
pub fn upsert_provider(
    state: tauri::State<'_, AppState>,
    mut profile: ProviderProfile,
    secret: Option<String>,
) -> Result<ProviderProfile, String> {
    if profile.id.trim().is_empty() {
        profile.id = format!("prof_{}", chrono::Utc::now().timestamp_millis());
    }
    if profile.created_at.trim().is_empty() {
        profile.created_at = chrono::Utc::now().to_rfc3339();
    }
    if let Some(s) = secret {
        if !s.is_empty() {
            providers::set_secret(&profile.id, &s)?;
        }
    }
    let mut cfg = state.config.lock().unwrap();
    match cfg.providers.iter_mut().find(|p| p.id == profile.id) {
        Some(existing) => *existing = profile.clone(),
        None => cfg.providers.push(profile.clone()),
    }
    config::save(&cfg).map_err(|e| e.to_string())?;
    Ok(profile)
}

/// Delete a provider profile (and its keyring secret).
#[tauri::command]
pub fn delete_provider(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    providers::delete_secret(&id);
    let mut cfg = state.config.lock().unwrap();
    cfg.providers.retain(|p| p.id != id);
    if cfg.active_provider_id.as_deref() == Some(&id) {
        cfg.active_provider_id = None;
    }
    config::save(&cfg).map_err(|e| e.to_string())
}

/// Activate a provider profile (`None` = use goosed's own config). Persists the
/// choice and restarts goosed so the provider env takes effect.
#[tauri::command]
pub async fn activate_provider(app: AppHandle, id: Option<String>) -> Result<(), String> {
    // Capture the previously-active Ollama model so we can evict it after the
    // switch (Round-2 item 5) — read before we overwrite active_provider_id.
    let prev_ollama = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        providers::active_ollama_target(&cfg)
    };
    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        if let Some(ref pid) = id {
            if !cfg.providers.iter().any(|p| &p.id == pid) {
                return Err("no such provider profile".into());
            }
        }
        cfg.active_provider_id = id;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    restart_goosed(app.clone()).await?;

    // Tell the frontend to re-sync provider state immediately (Round-2 item 4) —
    // without this the UI drifts until the next session create/load or health tick.
    let _ = app.emit("provider://activated", ());

    // Warm the new local Ollama model + evict the old one in the background
    // (Round-2 item 5) — don't make the switch wait on model load.
    let new_ollama = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        providers::active_ollama_target(&cfg)
    };
    tauri::async_runtime::spawn(async move {
        if let Some((base, model)) = &new_ollama {
            ollama::keep_alive_load(base, model).await;
        }
        if let Some((base, model)) = &prev_ollama {
            if Some((base, model)) != new_ollama.as_ref().map(|(b, m)| (b, m)) {
                ollama::keep_alive_release(base, model).await;
            }
        }
    });
    Ok(())
}
