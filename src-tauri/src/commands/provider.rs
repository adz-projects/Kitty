//! Provider profile commands: list/create/update/delete + activation (which
//! re-registers the provider with BigTiny and warms/evicts local Ollama
//! models around the switch).

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::config;
use crate::config::providers::{self, NetworkTier, ProviderProfile};
use crate::config::Config;
use crate::ollama;
use crate::openrouter;
use crate::state::AppState;

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
///
/// If the edited profile is the **currently active** provider, the change is
/// re-synced to BigTiny immediately (no restart / reactivate required) so
/// settings like `context_length` take effect on the next chat turn. Best-effort
/// on the daemon round-trip: the profile is already persisted, and a transient
/// daemon problem shouldn't fail the save — rebind/activate will re-sync later.
#[tauri::command]
pub async fn upsert_provider(
    app: AppHandle,
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
    let is_active;
    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        match cfg.providers.iter_mut().find(|p| p.id == profile.id) {
            Some(existing) => *existing = profile.clone(),
            None => cfg.providers.push(profile.clone()),
        }
        config::save(&cfg).map_err(|e| e.to_string())?;
        is_active = cfg.active_provider_id.as_deref() == Some(profile.id.as_str());
    }

    if is_active {
        if let Err(e) = crate::bigtiny::providers::sync_active_provider(&app).await {
            tracing::warn!(
                "provider {} edited but failed to re-sync to BigTiny: {e}",
                profile.id
            );
        }
    }
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

/// Best-effort context-length lookup for OpenRouter models, for the Providers
/// form's auto-suggest (Round-6 Feature 1). `Ok(None)` (not `Err`) when the
/// model isn't found in the list — this is a suggestion, not a required value.
#[tauri::command]
pub async fn openrouter_context_length(model: String) -> Result<Option<u32>, String> {
    let models = openrouter::list_models().await?;
    Ok(openrouter::context_length_for(&models, &model))
}

/// Check an OpenRouter provider profile's current credit balance/usage.
/// Reads the key from the keyring (never sent to/stored by Kitty otherwise)
/// — errors if the profile has no stored secret or isn't an OpenRouter profile.
#[tauri::command]
pub async fn openrouter_credits(
    state: tauri::State<'_, AppState>,
    provider_id: String,
) -> Result<serde_json::Value, String> {
    let profile = {
        let cfg = state.config.lock().unwrap();
        cfg.providers
            .iter()
            .find(|p| p.id == provider_id)
            .cloned()
            .ok_or("no such provider profile")?
    };
    if profile.provider_type != "openrouter" {
        return Err("not an OpenRouter profile".into());
    }
    let key = providers::get_secret_async(&provider_id)
        .await
        .ok_or("no API key stored for this profile — edit it and add one")?;
    openrouter::get_credits(&key).await
}

/// Manual, user-triggered re-check of the active provider (the chat view's
/// "can't reach" banner's Retry button) — reuses `test_connection` rather
/// than reintroducing any background polling. `Ok(())` when there's no active
/// provider (goosed's own config) — nothing for Kitty to check in that case.
#[tauri::command]
pub async fn test_active_provider_connection(
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let profile = {
        let cfg = state.config.lock().unwrap();
        cfg.active_provider_id
            .as_ref()
            .and_then(|id| cfg.providers.iter().find(|p| &p.id == id).cloned())
    };
    match profile {
        Some(p) => providers::test_connection(&p).await,
        None => Ok(()),
    }
}

/// Activate a provider profile. BigTiny has no built-in default and errors
/// any send with no provider registered, so `id: None` is rejected — a
/// provider must always be active. Health-gates the switch first (a
/// non-functioning target is rejected and the old provider stays active —
/// see `providers::test_connection`), then persists the choice and
/// re-registers it with BigTiny over REST (no daemon restart needed).
#[tauri::command]
pub async fn activate_provider(app: AppHandle, id: Option<String>) -> Result<(), String> {
    if id.is_none() {
        return Err("A provider must be active — add one in Settings → Providers.".to_string());
    }
    if let Some(ref pid) = id {
        let profile = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
            cfg.providers.iter().find(|p| &p.id == pid).cloned()
        };
        let profile = profile.ok_or("no such provider profile")?;
        providers::test_connection(&profile)
            .await
            .map_err(|e| format!("Can't switch to {} — {e}", profile.name))?;
    }

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
    // BigTiny switches providers at runtime over REST — no daemon restart.
    // Registration failure is a hard error.
    crate::bigtiny::providers::sync_active_provider(&app).await?;

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
