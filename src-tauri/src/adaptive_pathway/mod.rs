//! HTTP client for the Adaptive Pathway extension's sidecar — a separate
//! process this app spawns/supervises (see `lifecycle::adaptive_pathway_proc`).
//! Plain `reqwest` functions, mirroring `openrouter/mod.rs`'s style: Kitty
//! never talks to this over ACP (it isn't goosed), just plain REST/JSON on
//! `http://127.0.0.1:{port}`.

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::state::AppState;
use crate::util::http_client;

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The sidecar's base URL, derived from the configured port. Shared by every
/// caller (Settings commands, the auto-record-outcome backstop in
/// `goosed::stream`) so the lookup lives in exactly one place.
pub fn base_url(app: &AppHandle) -> String {
    let port = app
        .state::<AppState>()
        .config
        .lock()
        .unwrap()
        .adaptive_pathway_port;
    format!("http://127.0.0.1:{port}")
}

/// `GET /edges/{edge_id}` — the "why was this suggested" detail lookup.
pub async fn get_edge(base: &str, edge_id: &str) -> Result<Value, String> {
    let url = format!("{}/edges/{}", base.trim_end_matches('/'), edge_id);
    let resp = http_client()
        .get(url)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err("edge not found".into());
    }
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

/// `GET /state` — full system snapshot (status card, ensemble-weight sliders'
/// current values, schism-state polling).
pub async fn get_state(base: &str) -> Result<Value, String> {
    let url = format!("{}/state", base.trim_end_matches('/'));
    let resp = http_client()
        .get(url)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

/// `GET /metrics` — includes the `exploration_health` block (Graph Health's
/// new exploration-mix fields) that `/state` does not carry.
pub async fn get_metrics(base: &str) -> Result<Value, String> {
    let url = format!("{}/metrics", base.trim_end_matches('/'));
    let resp = http_client()
        .get(url)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

/// `POST /annotation?session_id=...` — powers the 👍👎💡🔄 feedback buttons.
pub async fn record_annotation(
    base: &str,
    session_id: &str,
    annotation_type: &str,
    edge_id: Option<&str>,
    action_id: Option<&str>,
    intensity: f32,
) -> Result<(), String> {
    let url = format!("{}/annotation", base.trim_end_matches('/'));
    let resp = http_client()
        .post(url)
        .query(&[("session_id", session_id)])
        .json(&json!({
            "type": annotation_type,
            "edge_id": edge_id,
            "action_id": action_id,
            "intensity": intensity,
        }))
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.error_for_status().map_err(|e| e.to_string())?;
    Ok(())
}

/// `POST /outcome?session_id=...` — the auto-record-outcome backstop (see
/// `goosed::stream::track_and_maybe_record_outcome`): fired whenever Kitty
/// observes a tool call complete over ACP, independent of whether the model
/// called `record_outcome` itself. No `context` — Kitty doesn't track a
/// per-session topic summary at the ACP-stream layer, so this degrades to
/// the sidecar's own zero/hashing fallback; that's an acceptable trade for a
/// best-effort backstop, not the primary (model-initiated, context-carrying)
/// signal path.
pub async fn record_outcome(
    base: &str,
    session_id: &str,
    action_id: &str,
    reward: f64,
) -> Result<(), String> {
    let url = format!("{}/outcome", base.trim_end_matches('/'));
    let resp = http_client()
        .post(url)
        .query(&[("session_id", session_id)])
        .json(&json!({
            "action_id": action_id,
            "reward": reward,
        }))
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.error_for_status().map_err(|e| e.to_string())?;
    Ok(())
}

/// `POST /suggestions/toggle?session_id=...&paused=...` — the pause/resume
/// header toggle.
pub async fn toggle_suggestions(base: &str, session_id: &str, paused: bool) -> Result<(), String> {
    let url = format!("{}/suggestions/toggle", base.trim_end_matches('/'));
    let resp = http_client()
        .post(url)
        .query(&[
            ("session_id", session_id.to_string()),
            ("paused", paused.to_string()),
        ])
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.error_for_status().map_err(|e| e.to_string())?;
    Ok(())
}

/// `GET /schism` — `{"state": "none"}` or full faction/agreement detail.
pub async fn get_schism(base: &str) -> Result<Value, String> {
    let url = format!("{}/schism", base.trim_end_matches('/'));
    let resp = http_client()
        .get(url)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

/// `POST /schism/resolve?keep_faction=a|b|both` — Schism Resolution modal actions.
pub async fn resolve_schism(base: &str, keep_faction: &str) -> Result<Value, String> {
    let url = format!("{}/schism/resolve", base.trim_end_matches('/'));
    let resp = http_client()
        .post(url)
        .query(&[("keep_faction", keep_faction)])
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct EnsembleWeightsBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    ig_weight_min: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ig_weight_max: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pc_weight: Option<f32>,
}

/// `PUT /config/ensemble` — the ensemble-weight sliders' writes (owner-requested
/// user-friendly surfacing of `ensemble.ig_weight_min`/`ig_weight_max`/`pc_weight`).
pub async fn update_ensemble_weights(
    base: &str,
    ig_weight_min: Option<f32>,
    ig_weight_max: Option<f32>,
    pc_weight: Option<f32>,
) -> Result<Value, String> {
    let url = format!("{}/config/ensemble", base.trim_end_matches('/'));
    let resp = http_client()
        .put(url)
        .json(&EnsembleWeightsBody {
            ig_weight_min,
            ig_weight_max,
            pc_weight,
        })
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

/// `GET /health` — Graph Health tab's issue list (Round-D Batch 2).
pub async fn health(base: &str) -> Result<Value, String> {
    let url = format!("{}/health", base.trim_end_matches('/'));
    let resp = http_client()
        .get(url)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

/// `GET /graph_health` — the richer `GraphHealth` struct (edge counts, tier
/// distribution, hotspots, override rate, …) behind the Graph Health tab
/// (Round-7 item 6) — distinct from `/health`'s issues-only payload above.
pub async fn get_graph_health(base: &str) -> Result<Value, String> {
    let url = format!("{}/graph_health", base.trim_end_matches('/'));
    let resp = http_client()
        .get(url)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    // Unlike the other GET helpers above, a non-2xx here (e.g. a sidecar
    // exe that predates this route and 404s) still parses as valid JSON
    // (FastAPI's `{"detail": "Not Found"}`) — checked explicitly so that
    // shape doesn't get treated as real `GraphHealth` data downstream.
    let resp = resp.error_for_status().map_err(|e| e.to_string())?;
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

/// `GET /domains` — Domain Profiles tab's list (Round-D Batch 2).
pub async fn list_domains(base: &str) -> Result<Value, String> {
    let url = format!("{}/domains", base.trim_end_matches('/'));
    let resp = http_client()
        .get(url)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct DomainUpdateBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dpp_diversity_weight: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    novelty_lambda: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    locked: Option<bool>,
}

/// `PUT /domains/{domain_id}` — Domain Profiles tab's edit action.
pub async fn update_domain(
    base: &str,
    domain_id: &str,
    name: Option<String>,
    dpp_diversity_weight: Option<f32>,
    novelty_lambda: Option<f32>,
    locked: Option<bool>,
) -> Result<Value, String> {
    let url = format!("{}/domains/{}", base.trim_end_matches('/'), domain_id);
    let resp = http_client()
        .put(url)
        .json(&DomainUpdateBody {
            name,
            dpp_diversity_weight,
            novelty_lambda,
            locked,
        })
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err("domain not found".into());
    }
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

/// `POST /nudge/accept?session_id=...` — the exploration-consent prompt's
/// Accept button. Returns `{"status", "active", "multiplier"}`.
pub async fn accept_nudge(base: &str, session_id: &str) -> Result<Value, String> {
    let url = format!("{}/nudge/accept", base.trim_end_matches('/'));
    let resp = http_client()
        .post(url)
        .query(&[("session_id", session_id)])
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

/// `POST /nudge/dismiss` — the exploration-consent prompt's Not now button
/// (no `session_id` param — the sidecar route doesn't take one).
pub async fn dismiss_nudge(base: &str) -> Result<(), String> {
    let url = format!("{}/nudge/dismiss", base.trim_end_matches('/'));
    let resp = http_client()
        .post(url)
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.error_for_status().map_err(|e| e.to_string())?;
    Ok(())
}

/// `GET /session_reflection?session_id=...` — the "see the roads not taken?"
/// session summary link.
pub async fn get_session_reflection(base: &str, session_id: &str) -> Result<Value, String> {
    let url = format!("{}/session_reflection", base.trim_end_matches('/'));
    let resp = http_client()
        .get(url)
        .query(&[("session_id", session_id)])
        .timeout(TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json::<Value>().await.map_err(|e| e.to_string())
}
