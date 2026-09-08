//! Specialist management commands.
//!
//! Every one proxies to the running daemon (`bigtiny::specialists`) — there is
//! no Kitty-side copy of a definition to keep in sync, which is the whole
//! reason this replaced `commands::recipes` rather than sitting beside it.

use crate::bigtiny::client::ensure_client;
use crate::bigtiny::specialists::{self, Specialist, SpecialistSpec};

#[tauri::command]
pub async fn list_specialists(app: tauri::AppHandle) -> Result<Vec<Specialist>, String> {
    let client = ensure_client(&app)?;
    specialists::list(&client).await
}

/// Create or edit. One command for both, because the daemon keys on `name`:
/// saving an existing name edits it, and saving a built-in's name creates this
/// app's override.
#[tauri::command]
pub async fn save_specialist(
    app: tauri::AppHandle,
    spec: SpecialistSpec,
) -> Result<String, String> {
    let client = ensure_client(&app)?;
    specialists::save(&client, &spec).await
}

#[tauri::command]
pub async fn delete_specialist(app: tauri::AppHandle, id: String) -> Result<(), String> {
    let client = ensure_client(&app)?;
    specialists::delete(&client, &id).await
}

/// Run a specialist from the UI, under an existing session.
///
/// The same orchestrator the model's own `call_specialist` goes through, so the
/// concurrency cap, depth limit and cancel propagation apply identically — a
/// user-triggered run is not a second, less-guarded path.
#[tauri::command]
pub async fn run_specialist(
    app: tauri::AppHandle,
    name: String,
    request: String,
    refs: Option<Vec<String>>,
    session_id: String,
) -> Result<serde_json::Value, String> {
    let client = ensure_client(&app)?;
    specialists::run(
        &client,
        &name,
        &request,
        &refs.unwrap_or_default(),
        &session_id,
    )
    .await
}

/// Tool names the Specialists form can offer as a checklist.
///
/// The daemon rejects a `tool_allow` naming a tool no server provides, so
/// offering a checklist rather than free text is what keeps that validation
/// from being the user's first feedback.
#[tauri::command]
pub async fn list_available_tools(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    let client = ensure_client(&app)?;
    specialists::available_tools(&client).await
}

/// What has been delegated recently, and how it went.
///
/// Routing depends entirely on each specialist's `description`, and a vague one
/// does not fail — it just gets used for the wrong things. Seeing which
/// specialist actually answered which request is the only way that becomes
/// visible.
#[tauri::command]
pub async fn list_specialist_runs(
    app: tauri::AppHandle,
) -> Result<Vec<crate::bigtiny::specialists::SpecialistRun>, String> {
    let client = ensure_client(&app)?;
    specialists::runs(&client).await
}

/// Populate the subagent model denylist from the catalog's premium tier, once.
///
/// Denied by default rather than after a surprising bill: the fallback chain's
/// last step before refusing is the parent's own model, and on a premium
/// profile that is exactly the model a user would not want a triage specialist
/// running on. Only the models this user actually has configured are listed —
/// a denylist naming hundreds of models they will never use is noise.
///
/// Runs once. `seeded` distinguishes "not asked yet" from "cleared
/// deliberately", so emptying the list is not silently undone next launch.
pub async fn seed_subagent_denylist(app: &tauri::AppHandle) {
    use tauri::Manager;

    let (already, profiles) = {
        let state = app.state::<crate::state::AppState>();
        let cfg = state.config.lock().unwrap();
        (cfg.specialists.seeded, cfg.providers.clone())
    };
    if already {
        return;
    }

    crate::openrouter::catalog::ensure_catalog_fresh(&app.state::<crate::state::AppState>()).await;
    let premium: Vec<String> = {
        let state = app.state::<crate::state::AppState>();
        let guard = match state.openrouter_catalog.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(catalog) = guard.as_ref() else {
            // No catalog yet (offline first run). Leave `seeded` false so this
            // is tried again rather than recording an empty list as a decision.
            return;
        };
        profiles
            .iter()
            .filter_map(|p| p.models.first())
            .filter(|m| {
                crate::openrouter::catalog::match_in_catalog(m, &catalog.entries).is_some_and(|e| {
                    matches!(
                        e.cost_tier,
                        Some(crate::openrouter::catalog::CostTier::Premium)
                    )
                })
            })
            .cloned()
            .collect()
    };

    let state = app.state::<crate::state::AppState>();
    let mut cfg = state.config.lock().unwrap();
    cfg.specialists.model_deny = premium;
    cfg.specialists.seeded = true;
    if let Err(e) = crate::config::save(&cfg) {
        tracing::warn!("could not save the seeded subagent denylist: {e}");
    }
}
