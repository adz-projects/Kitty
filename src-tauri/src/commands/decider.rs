//! Settings → Helper Models → Decision model.
//!
//! The settings live in Kitty's config; `lifecycle::decider` relays them to
//! the engine and `bigtiny::mcp` to kitty-web. Testing an endpoint goes through
//! the engine (`/api/apps/me/decider/probe`), so it measures the same path the
//! features will use, on both platforms.

use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::config::providers::{delete_secret, get_secret_async, set_secret_async};
use crate::config::{self, DeciderSettings};
use crate::state::AppState;

/// Keychain ids for the decision models' API keys (OS keychain, like provider
/// secrets — never config.json). The primary and the optional fallback each
/// have their own slot.
pub const DECIDER_KEY_MAIN: &str = "decider-main";
pub const DECIDER_KEY_BACKUP: &str = "decider-backup";

/// Apply a key edit to a keychain slot: `Some(key)` sets it, `Some("")` clears
/// it, `None` leaves it unchanged (the form didn't touch the field).
async fn apply_key(slot: &str, value: Option<String>) {
    match value {
        Some(k) if !k.trim().is_empty() => {
            if let Err(e) = set_secret_async(slot, k.trim()).await {
                tracing::warn!("could not store decision-model key for {slot}: {e}");
            }
        }
        Some(_) => delete_secret(slot),
        None => {}
    }
}

#[tauri::command]
pub fn get_decider_settings(app: AppHandle) -> DeciderSettings {
    app.state::<AppState>().config.lock().unwrap().decider.clone()
}

/// Whether an API key is stored for the main / backup decision model, so the
/// form can show "key set" without ever revealing it.
#[tauri::command]
pub async fn get_decider_key_set() -> (bool, bool) {
    (
        get_secret_async(DECIDER_KEY_MAIN).await.is_some(),
        get_secret_async(DECIDER_KEY_BACKUP).await.is_some(),
    )
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
    api_key: Option<String>,
    secondary_api_key: Option<String>,
) -> Result<DeciderSettings, String> {
    // Secrets go to the OS keychain, never config.json (same as providers).
    apply_key(DECIDER_KEY_MAIN, api_key).await;
    apply_key(DECIDER_KEY_BACKUP, secondary_api_key).await;
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
        // The question set is edited in config.json, never in Settings, so a
        // save from the Settings form (which does not carry it) keeps it.
        if settings.paragraph_screen.is_none() {
            settings.paragraph_screen = old.paragraph_screen.clone();
        }
        cfg.decider = settings.clone();
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    crate::lifecycle::decider::apply_decider(&app).await;
    super::mcp_servers::apply(&app, "kitty-web", "the web checks").await?;
    Ok(settings)
}

/// `{ok, model, rtt_ms, error}` for an endpoint, without saving it. `api_key`
/// and `model` let a hosted endpoint (gateway / Cloudflare) be tested with its
/// bearer token and model id before saving.
#[tauri::command]
pub async fn test_decider(
    app: AppHandle,
    dialect: String,
    url: String,
    api_key: Option<String>,
    model: Option<String>,
) -> Result<Value, String> {
    let client = crate::bigtiny::client::ensure_client(&app)?;
    let mut body = serde_json::json!({ "dialect": dialect, "url": url.trim() });
    if let Some(k) = api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        body["api_key"] = Value::String(k.to_string());
    }
    if let Some(m) = model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        body["model"] = Value::String(m.to_string());
    }
    client
        .post_json("/api/apps/me/decider/probe", &body)
        .await
        .map_err(|e| {
            tracing::warn!("decision-model probe failed: {e}");
            "Could not reach the engine to test the decision model.".to_string()
        })
}

/// Batch size when chunking is on: several passages per decision-model call.
/// 1 = off (one call per paragraph). 4 was the best precision/throughput point
/// in the accuracy eval (≈89% agreement with single-screening, 4× fewer calls).
const CHUNK_BATCH_ON: i64 = 4;

/// The engine's built-in paragraph-screen set (`durable-substantive-v1`),
/// materialized here when chunking is toggled and the user has no custom set in
/// config.json, so the toggle has a complete `paragraph_screen` to attach
/// `batch_size` to. Kept in sync with the engine default.
fn default_screen_set() -> Value {
    serde_json::json!({
        "id": "durable-substantive-v1",
        "questions": [
            {"name": "durable", "keep_at": 0.6, "instructions":
                "Will this passage still be informative in a few years, after the specific products, prices, people or events it mentions are no longer current? Answer yes for explanations of how or why things work, lasting history, definitions and durable relationships; answer no for specs, prices, release dates, rankings, schedules, status updates and announcements."},
            {"name": "substantive", "keep_at": 0.6, "instructions":
                "Is this passage substantive and well supported (concrete claims, mechanisms, evidence or reasoned argument) rather than marketing copy, filler, unsupported speculation or vague generalities?"}
        ]
    })
}

/// Turn memory-intake chunking on or off: on screens several passages per
/// decision-model call (cheaper, but admits a little more filler); off is one
/// call per paragraph. Flips `paragraph_screen.batch_size`, preserving any
/// custom question set.
#[tauri::command]
pub async fn set_decider_chunking(app: AppHandle, on: bool) -> Result<(), String> {
    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        let mut ps = cfg.decider.paragraph_screen.clone().unwrap_or_else(default_screen_set);
        ps["batch_size"] = Value::from(if on { CHUNK_BATCH_ON } else { 1 });
        cfg.decider.paragraph_screen = Some(ps);
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    crate::lifecycle::decider::apply_decider(&app).await;
    Ok(())
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
