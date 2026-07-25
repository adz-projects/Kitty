//! Commands for the Adaptive Pathway extension's HTTP sidecar: process
//! lifecycle (status/restart/enable) plus the handful of sidecar endpoints
//! Kitty's chat surface and Settings actually need — see
//! `crate::adaptive_pathway` for the plain-HTTP client functions and
//! `crate::lifecycle::adaptive_pathway_proc` for process supervision.

use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::config;
use crate::lifecycle;
use crate::lifecycle::adaptive_pathway_proc::{self, AdaptivePathwayStatus, EmbeddingModelStatus};
use crate::state::AppState;

fn ap_base(app: &AppHandle) -> String {
    crate::adaptive_pathway::base_url(app)
}

/// Every HTTP-backed command below short-circuits here rather than letting a
/// dead sidecar hang the caller with a confusing connect-timeout error.
fn require_ok(app: &AppHandle) -> Result<(), String> {
    let status = *app
        .state::<AppState>()
        .adaptive_pathway_status
        .lock()
        .unwrap();
    if status == AdaptivePathwayStatus::Ok {
        Ok(())
    } else {
        Err("Adaptive Pathway isn't running — check Settings → Adaptive Pathway.".into())
    }
}

#[tauri::command]
pub fn get_adaptive_pathway_status(app: AppHandle) -> Result<AdaptivePathwayStatus, String> {
    Ok(*app
        .state::<AppState>()
        .adaptive_pathway_status
        .lock()
        .unwrap())
}

/// Readiness of the shared `qwen3-embedding:0.6b` model — surfaced
/// separately from `get_adaptive_pathway_status` (see `EmbeddingModelStatus`)
/// so Settings → Adaptive Pathway can show "downloading the embedding
/// model" without it reading as the sidecar itself being down.
#[tauri::command]
pub fn get_adaptive_pathway_embedding_status(
    app: AppHandle,
) -> Result<EmbeddingModelStatus, String> {
    Ok(*app
        .state::<AppState>()
        .adaptive_pathway_embedding_status
        .lock()
        .unwrap())
}

/// Restart the sidecar if we own the process (else the user must restart it).
#[tauri::command]
pub async fn restart_adaptive_pathway(app: AppHandle) -> Result<(), String> {
    let (launch_command, launch_args, db_path, port, embedding_model, embedding_url) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        (
            cfg.adaptive_pathway_launch_command.clone(),
            cfg.adaptive_pathway_launch_args.clone(),
            cfg.adaptive_pathway_db_path.clone(),
            cfg.adaptive_pathway_port,
            cfg.adaptive_pathway_embedding_model.clone(),
            cfg.ollama_base_url.clone(),
        )
    };
    {
        let state = app.state::<AppState>();
        let mut proc = state.adaptive_pathway.lock().unwrap();
        if !proc.owned {
            return Err("Adaptive Pathway is running externally — restart it yourself.".into());
        }
        proc.kill_if_owned();
    }
    *app.state::<AppState>()
        .adaptive_pathway_status
        .lock()
        .unwrap() = AdaptivePathwayStatus::Starting;
    let proc = adaptive_pathway_proc::ensure_running(
        &launch_command,
        &launch_args,
        &db_path,
        port,
        &embedding_model,
        &embedding_url,
    )
    .await?;
    let client = crate::util::http_client();
    let up =
        adaptive_pathway_proc::probe_health(&client, &format!("http://127.0.0.1:{port}")).await;
    let state = app.state::<AppState>();
    *state.adaptive_pathway.lock().unwrap() = proc;
    *state.adaptive_pathway_status.lock().unwrap() = if up {
        AdaptivePathwayStatus::Ok
    } else {
        AdaptivePathwayStatus::Down
    };
    Ok(())
}

/// Enable/disable the feature: persists config, then spawns (if turning on) or
/// kills-if-owned (if turning off) — no full app restart required.
#[tauri::command]
pub async fn set_adaptive_pathway_enabled(app: AppHandle, enabled: bool) -> Result<(), String> {
    let (launch_command, launch_args, db_path, port, embedding_model, embedding_url) = {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        cfg.adaptive_pathway_enabled = enabled;
        config::save(&cfg).map_err(|e| e.to_string())?;
        (
            cfg.adaptive_pathway_launch_command.clone(),
            cfg.adaptive_pathway_launch_args.clone(),
            cfg.adaptive_pathway_db_path.clone(),
            cfg.adaptive_pathway_port,
            cfg.adaptive_pathway_embedding_model.clone(),
            cfg.ollama_base_url.clone(),
        )
    };

    if enabled {
        *app.state::<AppState>()
            .adaptive_pathway_status
            .lock()
            .unwrap() = AdaptivePathwayStatus::Starting;
        let proc = adaptive_pathway_proc::ensure_running(
            &launch_command,
            &launch_args,
            &db_path,
            port,
            &embedding_model,
            &embedding_url,
        )
        .await?;
        let client = crate::util::http_client();
        let up =
            adaptive_pathway_proc::probe_health(&client, &format!("http://127.0.0.1:{port}")).await;
        let state = app.state::<AppState>();
        *state.adaptive_pathway.lock().unwrap() = proc;
        *state.adaptive_pathway_status.lock().unwrap() = if up {
            AdaptivePathwayStatus::Ok
        } else {
            AdaptivePathwayStatus::Down
        };

        // Toggling AP on at runtime (not just app startup) must provision
        // Ollama + the embedding model the same way `start_stack` does —
        // otherwise a user who enables it mid-session from Settings never
        // gets embeddings until the next full app restart. Reuses
        // `ensure_ollama_running`'s already-owned guard rather than calling
        // `ollama_proc::ensure_running` directly here, so a live owned
        // handle from `start_stack` never gets silently overwritten/leaked.
        let _ = super::ollama::ensure_ollama_running(app.clone()).await;
        lifecycle::ensure_embedding_model(app.clone(), embedding_url, embedding_model).await;
    } else {
        let state = app.state::<AppState>();
        state.adaptive_pathway.lock().unwrap().kill_if_owned();
        *state.adaptive_pathway_status.lock().unwrap() = AdaptivePathwayStatus::Disabled;
    }
    // Keep the BigTiny MCP-server registration for the `decide`/
    // `record_outcome` tools in sync with the toggle above — mirrors the
    // sidecar process start/stop we just did.
    crate::bigtiny::mcp::ensure_builtin_servers(&app).await;
    Ok(())
}

/// "Why was this suggested" — resolves a hint's `edge_id` to full edge detail.
#[tauri::command]
pub async fn adaptive_pathway_get_edge(app: AppHandle, edge_id: String) -> Result<Value, String> {
    require_ok(&app)?;
    crate::adaptive_pathway::get_edge(&ap_base(&app), &edge_id).await
}

/// Settings status card + ensemble-weight sliders' current values + Graph Health.
#[tauri::command]
pub async fn adaptive_pathway_get_state(app: AppHandle) -> Result<Value, String> {
    require_ok(&app)?;
    crate::adaptive_pathway::get_state(&ap_base(&app)).await
}

/// Graph Health's `exploration_health` block — `/state` doesn't carry this.
#[tauri::command]
pub async fn adaptive_pathway_get_metrics(app: AppHandle) -> Result<Value, String> {
    require_ok(&app)?;
    crate::adaptive_pathway::get_metrics(&ap_base(&app)).await
}

/// 👍👎💡🔄 feedback buttons.
#[tauri::command]
pub async fn adaptive_pathway_record_annotation(
    app: AppHandle,
    session_id: String,
    annotation_type: String,
    edge_id: Option<String>,
    action_id: Option<String>,
    intensity: f32,
) -> Result<(), String> {
    require_ok(&app)?;
    crate::adaptive_pathway::record_annotation(
        &ap_base(&app),
        &session_id,
        &annotation_type,
        edge_id.as_deref(),
        action_id.as_deref(),
        intensity,
    )
    .await
}

/// Pause/resume header toggle.
#[tauri::command]
pub async fn adaptive_pathway_toggle_suggestions(
    app: AppHandle,
    session_id: String,
    paused: bool,
) -> Result<(), String> {
    require_ok(&app)?;
    crate::adaptive_pathway::toggle_suggestions(&ap_base(&app), &session_id, paused).await
}

/// Schism Resolution modal detail.
#[tauri::command]
pub async fn adaptive_pathway_get_schism(app: AppHandle) -> Result<Value, String> {
    require_ok(&app)?;
    crate::adaptive_pathway::get_schism(&ap_base(&app)).await
}

/// Schism Resolution modal actions (`"a"` | `"b"` | `"both"`).
#[tauri::command]
pub async fn adaptive_pathway_resolve_schism(
    app: AppHandle,
    keep_faction: String,
) -> Result<Value, String> {
    require_ok(&app)?;
    crate::adaptive_pathway::resolve_schism(&ap_base(&app), &keep_faction).await
}

/// Ensemble-weight sliders' writes (owner-requested surfacing of
/// `ensemble.ig_weight_min`/`ig_weight_max`/`pc_weight`).
#[tauri::command]
pub async fn adaptive_pathway_update_ensemble_weights(
    app: AppHandle,
    ig_weight_min: Option<f32>,
    ig_weight_max: Option<f32>,
    pc_weight: Option<f32>,
) -> Result<Value, String> {
    require_ok(&app)?;
    crate::adaptive_pathway::update_ensemble_weights(
        &ap_base(&app),
        ig_weight_min,
        ig_weight_max,
        pc_weight,
    )
    .await
}

/// Graph Health tab's issue list.
#[tauri::command]
pub async fn adaptive_pathway_health(app: AppHandle) -> Result<Value, String> {
    require_ok(&app)?;
    crate::adaptive_pathway::health(&ap_base(&app)).await
}

/// Graph Health tab's richer data (edge counts, tier distribution, hotspots,
/// override rate, …) — distinct from `adaptive_pathway_health`'s issues-only
/// payload above.
#[tauri::command]
pub async fn adaptive_pathway_graph_health(app: AppHandle) -> Result<Value, String> {
    require_ok(&app)?;
    crate::adaptive_pathway::get_graph_health(&ap_base(&app)).await
}

/// Domain Profiles tab's list.
#[tauri::command]
pub async fn adaptive_pathway_list_domains(app: AppHandle) -> Result<Value, String> {
    require_ok(&app)?;
    crate::adaptive_pathway::list_domains(&ap_base(&app)).await
}

/// Domain Profiles tab's edit action.
#[tauri::command]
pub async fn adaptive_pathway_update_domain(
    app: AppHandle,
    domain_id: String,
    name: Option<String>,
    dpp_diversity_weight: Option<f32>,
    novelty_lambda: Option<f32>,
    locked: Option<bool>,
) -> Result<Value, String> {
    require_ok(&app)?;
    crate::adaptive_pathway::update_domain(
        &ap_base(&app),
        &domain_id,
        name,
        dpp_diversity_weight,
        novelty_lambda,
        locked,
    )
    .await
}

/// Exploration-consent prompt's Accept button.
#[tauri::command]
pub async fn adaptive_pathway_accept_nudge(
    app: AppHandle,
    session_id: String,
) -> Result<Value, String> {
    require_ok(&app)?;
    crate::adaptive_pathway::accept_nudge(&ap_base(&app), &session_id).await
}

/// Exploration-consent prompt's Not now button.
#[tauri::command]
pub async fn adaptive_pathway_dismiss_nudge(app: AppHandle) -> Result<(), String> {
    require_ok(&app)?;
    crate::adaptive_pathway::dismiss_nudge(&ap_base(&app)).await
}

/// "See the roads not taken?" session-footer link.
#[tauri::command]
pub async fn adaptive_pathway_get_session_reflection(
    app: AppHandle,
    session_id: String,
) -> Result<Value, String> {
    require_ok(&app)?;
    crate::adaptive_pathway::get_session_reflection(&ap_base(&app), &session_id).await
}
