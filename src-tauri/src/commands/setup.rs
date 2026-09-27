//! First-run wizard + Setup & Repair commands. Named `setup` (not `wizard`) to
//! avoid colliding with the top-level `crate::wizard` module this file wraps.

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::config;
use crate::config::providers::get_secret_async;
use crate::lifecycle;
use crate::state::AppState;
use crate::state::StackStatus;
use crate::windows;
// Only the autostart commands below use it, and those are Windows-only.
#[cfg(windows)]
use crate::wizard;

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
    /// Where repair should open (#63): the first wizard step that is not
    /// right, or `None` when nothing is.
    pub first_broken_step: Option<SetupStep>,
}

/// The wizard steps repair can open at, in wizard order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupStep {
    /// The engine is not running.
    Engine,
    /// No usable default provider, no model on it, or a missing key.
    Provider,
    /// A helper model the user chose (memory or local summarizer) is not on
    /// disk. Optional, so it never makes setup "not ready".
    Models,
}

/// Pure, so the order is testable.
fn first_broken(engine_ok: bool, provider_ok: bool, models_ok: bool) -> Option<SetupStep> {
    if !engine_ok {
        Some(SetupStep::Engine)
    } else if !provider_ok {
        Some(SetupStep::Provider)
    } else if !models_ok {
        Some(SetupStep::Models)
    } else {
        None
    }
}

/// Check whether the active provider + stack are actually ready to chat:
/// a model is selected (and, for a remote provider, a key is stored), and
/// the stack itself (the daemon, plus a local model when the active path
/// needs one) reports healthy. Used by the wizard's Done step and by Settings → Setup &
/// Repair's lighter re-check — both just want a yes/no plus why not.
#[tauri::command]
pub async fn validate_setup(app: AppHandle) -> Result<SetupValidation, String> {
    let mut issues = Vec::new();

    let (active_provider, ap_enabled, helper_models_ok) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        let active = cfg
            .active_provider_id
            .as_ref()
            .and_then(|id| cfg.providers.iter().find(|p| &p.id == id).cloned())
            .filter(|p| p.is_usable());
        let memory = state.memory_status.lock().unwrap();
        let wants_memory = cfg.adaptive_pathway_enabled
            || (cfg.memorabilia_enabled && memory.memorabilia_supported);
        let memory_ok = !wants_memory || memory.model_installed;
        let summarizer_ok =
            !cfg.summarizer.enabled || crate::models::resolve(&cfg.summarizer.model).is_some();
        (
            active,
            cfg.adaptive_pathway_enabled,
            memory_ok && summarizer_ok,
        )
    };

    match &active_provider {
        None => issues.push("No model or provider is set up yet.".into()),
        Some(p) => {
            if p.models.is_empty() {
                issues.push(format!("\"{}\" doesn't have a model selected yet.", p.name));
            }
            // `get_secret_async` (not the blocking `has_secret`) — this is a
            // tokio worker, and Windows Credential Manager access is
            // synchronous OS IPC that would otherwise block it. A keyless
            // provider (a self-hosted endpoint) is fine without one (#64).
            if p.requires_key() && get_secret_async(&p.id).await.is_none() {
                issues.push(format!("\"{}\" doesn't have an API key stored.", p.name));
            }
        }
    }
    let provider_ok = issues.is_empty();

    let client = crate::util::http_client();
    let status = lifecycle::compute_status(&app, &client).await;
    match status {
        StackStatus::Ok => {}
        StackStatus::Starting => issues.push("Still starting up — try again in a moment.".into()),
        StackStatus::BackendDown => issues.push("Kitty's engine isn't running yet.".into()),
    }

    // The pathway engine runs in-process inside BigTiny now — there's no
    // separate sidecar to probe. "Ok" just means enabled and the daemon
    // itself is reachable; unlike chat readiness, this doesn't care about
    // model/provider status (a missing embedding model lowers recall quality,
    // it doesn't stop the engine).
    let adaptive_pathway_ok = ap_enabled && status != StackStatus::BackendDown;

    Ok(SetupValidation {
        ready: issues.is_empty(),
        first_broken_step: first_broken(status == StackStatus::Ok, provider_ok, helper_models_ok),
        issues,
        adaptive_pathway_ok,
    })
}

/// Open the wizard in `"setup"` or `"repair"` mode.
#[tauri::command]
pub async fn open_wizard(app: AppHandle, mode: Option<String>) -> Result<(), String> {
    windows::open_wizard(&app, mode.as_deref().unwrap_or("setup")).map_err(|e| e.to_string())
}

/// Mark first-run setup complete, then summon the overlay.
#[tauri::command]
pub async fn complete_setup(app: AppHandle) -> Result<(), String> {
    // Copy the mutated config out of the lock and release it before
    // `save`'s synchronous disk write — holding the global config Mutex
    // across a disk write would block every other config-reading command.
    let updated = {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        cfg.setup_completed = true;
        cfg.clone()
    };
    config::save(&updated).map_err(|e| e.to_string())?;
    // The wizard is a hub route now, not a window to hide — the frontend
    // routes itself back to chat when this resolves. Summoning the overlay
    // stays: it's how first run hands the user their first prompt.
    windows::show_overlay(&app).map_err(|e| e.to_string())
}

// Autostart is the HKCU Run key — Windows-only, with no Android v1
// equivalent (docs/ANDROID.md D23). `lib.rs` gates the handler entries to
// match; the Settings → General toggle is hidden on Android in Phase 6b.
#[cfg(windows)]
#[tauri::command]
pub fn get_autostart() -> Result<bool, String> {
    Ok(wizard::autostart_enabled())
}

#[cfg(windows)]
#[tauri::command]
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    wizard::set_autostart(enabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_opens_at_the_first_broken_step_in_wizard_order() {
        assert_eq!(first_broken(false, false, false), Some(SetupStep::Engine));
        assert_eq!(first_broken(true, false, false), Some(SetupStep::Provider));
        assert_eq!(first_broken(true, true, false), Some(SetupStep::Models));
        assert_eq!(first_broken(true, true, true), None);
    }

    #[test]
    fn self_hosted_endpoints_need_no_key() {
        let profile = |provider_type: &str| -> crate::config::providers::ProviderProfile {
            serde_json::from_value(serde_json::json!({
                "id": "p", "name": "P", "provider_type": provider_type,
                "base_url": "", "created_at": ""
            }))
            .unwrap()
        };
        assert!(!profile("custom_openai").requires_key());
        assert!(!profile("ollama").requires_key());
        assert!(profile("openrouter").requires_key());
        assert!(profile("anthropic").requires_key());
    }
}
