//! First-run wizard + Setup & Repair commands. Named `setup` (not `wizard`) to
//! avoid colliding with the top-level `crate::wizard` detection/install module
//! this file wraps.

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::config;
use crate::config::providers::has_secret;
use crate::lifecycle;
use crate::state::AppState;
use crate::state::StackStatus;
use crate::windows;
use crate::wizard;

use super::ollama::ollama_base;

/// Detect Ollama + Goose (presence, version, path).
#[tauri::command]
pub async fn detect_dependencies(app: AppHandle) -> Result<wizard::Detection, String> {
    let base = ollama_base(&app);
    let goose_override = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        cfg.goose_binary_override.clone()
    };
    Ok(wizard::detect(&base, goose_override.as_deref()).await)
}

/// Install a dependency (`ollama` — downloads+runs its official installer;
/// `goose` — downloads+extracts its CLI zip and records the path, since it
/// has no installer at all). See `wizard::install`. Manually pointing at an
/// existing `goose.exe` (the fallback next to one-click install) needs no
/// dedicated command — the frontend picks the file via the existing
/// `pickFiles()`/dialog-plugin pattern and writes `goose_binary_override`
/// through the generic `get_config`/`set_config` commands, same as every
/// other plain config field (e.g. `default_context_folder`).
#[tauri::command]
pub async fn install_dependency(app: AppHandle, which: String) -> Result<(), String> {
    wizard::install(&app, &which).await
}

/// Result of `validate_setup`: whether the current setup (whichever path the
/// wizard's fork led down) actually works, plus plain-language reasons when
/// it doesn't. Powers the wizard's Done-step summary and its soft
/// Finish-anyway gate, and Setup & Repair's lighter re-check.
/// `adaptive_pathway_ok` is reported separately, never folded into `ready`/
/// `issues` — it's an optional augmentation, not a chat-blocking requirement
/// (same "quietly Down is fine" philosophy as everywhere else it appears).
#[derive(Debug, Clone, Serialize)]
pub struct SetupValidation {
    pub ready: bool,
    pub issues: Vec<String>,
    pub adaptive_pathway_ok: bool,
}

/// Check whether the active provider + stack are actually ready to chat:
/// a model is selected (and, for a remote provider, a key is stored), and
/// the stack itself (goosed, plus Ollama when the active path needs it)
/// reports healthy. Used by the wizard's Done step and by Settings → Setup &
/// Repair's lighter re-check — both just want a yes/no plus why not.
#[tauri::command]
pub async fn validate_setup(app: AppHandle) -> Result<SetupValidation, String> {
    let mut issues = Vec::new();

    let (active_provider, ap_enabled, ap_port) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        let active = cfg
            .active_provider_id
            .as_ref()
            .and_then(|id| cfg.providers.iter().find(|p| &p.id == id).cloned());
        (
            active,
            cfg.adaptive_pathway_enabled,
            cfg.adaptive_pathway_port,
        )
    };

    match &active_provider {
        None => issues.push("No model or provider is set up yet.".into()),
        Some(p) => {
            if p.models.is_empty() {
                issues.push(format!("\"{}\" doesn't have a model selected yet.", p.name));
            }
            if p.provider_type != "ollama" && !has_secret(&p.id) {
                issues.push(format!("\"{}\" doesn't have an API key stored.", p.name));
            }
        }
    }

    let client = crate::util::http_client();
    match lifecycle::compute_status(&app, &client, false).await {
        StackStatus::Ok | StackStatus::ConflictGooseDesktop => {}
        StackStatus::Starting => issues.push("Still starting up — try again in a moment.".into()),
        StackStatus::OllamaDown => issues.push("Ollama isn't running.".into()),
        StackStatus::GoosedDown => issues.push("Goose isn't running yet.".into()),
        StackStatus::NoModel => issues.push("No Ollama model is installed yet.".into()),
        StackStatus::ProviderUnreachable => {
            issues.push("Can't reach the active provider right now.".into())
        }
    }

    let adaptive_pathway_ok = ap_enabled
        && crate::lifecycle::adaptive_pathway_proc::probe_health(
            &client,
            &format!("http://127.0.0.1:{ap_port}"),
        )
        .await;

    Ok(SetupValidation {
        ready: issues.is_empty(),
        issues,
        adaptive_pathway_ok,
    })
}

/// Open the wizard in `"setup"` or `"repair"` mode.
#[tauri::command]
pub async fn open_wizard(app: AppHandle, mode: Option<String>) -> Result<(), String> {
    windows::open_wizard(&app, mode.as_deref().unwrap_or("setup")).map_err(|e| e.to_string())
}

/// The wizard launch mode the window should read on open.
#[tauri::command]
pub fn get_wizard_mode(state: tauri::State<'_, AppState>) -> Result<Option<String>, String> {
    Ok(state.wizard_mode.lock().unwrap().clone())
}

/// Mark first-run setup complete, then summon the overlay.
#[tauri::command]
pub async fn complete_setup(app: AppHandle) -> Result<(), String> {
    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        cfg.setup_completed = true;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    if let Some(win) = app.get_webview_window(windows::WIZARD) {
        let _ = win.hide();
    }
    windows::show_overlay(&app).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_autostart() -> Result<bool, String> {
    Ok(wizard::autostart_enabled())
}

#[tauri::command]
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    wizard::set_autostart(enabled)
}
