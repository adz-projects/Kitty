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
}

/// Check whether the active provider + stack are actually ready to chat:
/// a model is selected (and, for a remote provider, a key is stored), and
/// the stack itself (the daemon, plus a local model when the active path
/// needs one) reports healthy. Used by the wizard's Done step and by Settings → Setup &
/// Repair's lighter re-check — both just want a yes/no plus why not.
#[tauri::command]
pub async fn validate_setup(app: AppHandle) -> Result<SetupValidation, String> {
    let mut issues = Vec::new();

    let (active_provider, ap_enabled) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        let active = cfg
            .active_provider_id
            .as_ref()
            .and_then(|id| cfg.providers.iter().find(|p| &p.id == id).cloned());
        (active, cfg.adaptive_pathway_enabled)
    };

    match &active_provider {
        None => issues.push("No model or provider is set up yet.".into()),
        Some(p) => {
            if p.models.is_empty() {
                issues.push(format!("\"{}\" doesn't have a model selected yet.", p.name));
            }
            // `get_secret_async` (not the blocking `has_secret`) — this is a
            // tokio worker, and Windows Credential Manager access is
            // synchronous OS IPC that would otherwise block it.
            if p.provider_type != "ollama" && get_secret_async(&p.id).await.is_none() {
                issues.push(format!("\"{}\" doesn't have an API key stored.", p.name));
            }
        }
    }

    let client = crate::util::http_client();
    let status = lifecycle::compute_status(&app, &client).await;
    match status {
        StackStatus::Ok => {}
        StackStatus::Starting => issues.push("Still starting up — try again in a moment.".into()),
        StackStatus::BackendDown => issues.push("Kitty's engine isn't running yet.".into()),
        StackStatus::LocalModelMissing => {
            issues.push("No local model is downloaded yet.".into())
        }
        StackStatus::ProviderUnreachable => {
            issues.push("Can't reach the active provider right now.".into())
        }
    }

    // The pathway engine runs in-process inside BigTiny now — there's no
    // separate sidecar to probe. "Ok" just means enabled and the daemon
    // itself is reachable; unlike chat readiness, this doesn't care about
    // model/provider status (a missing embedding model lowers recall quality,
    // it doesn't stop the engine).
    let adaptive_pathway_ok = ap_enabled && status != StackStatus::BackendDown;

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
