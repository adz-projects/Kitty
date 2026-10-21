//! Settings → Helper Models → Decision model.
//!
//! The settings live in Kitty's config; `lifecycle::decider` relays them to
//! the engine and `bigtiny::mcp` to kitty-web. Testing an endpoint goes through
//! the engine (`/api/apps/me/decider/probe`), so it measures the same path the
//! features will use, on both platforms.

use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::config::{self, DeciderSettings};
use crate::state::AppState;

#[tauri::command]
pub fn get_decider_settings(app: AppHandle) -> DeciderSettings {
    app.state::<AppState>().config.lock().unwrap().decider.clone()
}

/// Save the settings and apply them at once: the engine's per-app copy, then
/// kitty-web's row (which `patch_config` would not re-sync).
///
/// When the model changes, the user's threshold overrides are dropped — they
/// were tuned for the old model — unless this same save also edits them.
#[tauri::command]
pub async fn set_decider_settings(
    app: AppHandle,
    mut settings: DeciderSettings,
) -> Result<DeciderSettings, String> {
    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        let old = &cfg.decider;
        let model_changed = old.model.is_some()
            && settings.model.is_some()
            && old.model != settings.model;
        if model_changed && settings.thresholds == old.thresholds {
            settings.thresholds.clear();
        }
        cfg.decider = settings.clone();
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    crate::lifecycle::decider::apply_decider(&app).await;
    super::mcp_servers::apply(&app, "kitty-web", "the web checks").await?;
    Ok(settings)
}

/// `{ok, model, rtt_ms, error}` for an endpoint, without saving it.
#[tauri::command]
pub async fn test_decider(app: AppHandle, dialect: String, url: String) -> Result<Value, String> {
    let client = crate::bigtiny::client::ensure_client(&app)?;
    client
        .post_json(
            "/api/apps/me/decider/probe",
            &serde_json::json!({ "dialect": dialect, "url": url.trim() }),
        )
        .await
        .map_err(|e| {
            tracing::warn!("decision-model probe failed: {e}");
            "Could not reach the engine to test the decision model.".to_string()
        })
}

/// Per-feature counters since the engine started.
#[tauri::command]
pub async fn get_decider_stats(app: AppHandle) -> Result<Value, String> {
    let client = crate::bigtiny::client::ensure_client(&app)?;
    client
        .get_json("/api/apps/me/decider/stats")
        .await
        .map_err(|e| {
            tracing::warn!("decision-model stats failed: {e}");
            "Could not read the decision-model counters.".to_string()
        })
}

/// The engine's default thresholds, for the Thresholds section's placeholders.
#[tauri::command]
pub async fn get_decider_defaults(app: AppHandle) -> Result<Value, String> {
    let client = crate::bigtiny::client::ensure_client(&app)?;
    let v = client
        .get_json("/api/apps/me/decider")
        .await
        .map_err(|e| {
            tracing::warn!("decision-model defaults failed: {e}");
            "Could not read the decision-model defaults.".to_string()
        })?;
    Ok(serde_json::json!({
        "dialects": v.get("dialects").cloned().unwrap_or(Value::Null),
        "default_thresholds": v.get("default_thresholds").cloned().unwrap_or(Value::Null),
    }))
}
