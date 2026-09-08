//! `/api/specialists` — the definitions behind `call_specialist`.
//!
//! Two ownership rules, both inherited from elsewhere in the daemon rather than
//! invented here:
//!
//! * A built-in (`app_id IS NULL`) is visible to everyone and modifiable by no
//!   one — 403, exactly as a shared `mcp_servers` row is, and for the same
//!   reason: it is what *every* app's agent loop would run.
//! * An app that wants a different researcher POSTs its own, which shadows the
//!   built-in for that app alone. `DELETE` on it reverts to the built-in, which
//!   is the same revert-by-delete shape as `/api/apps/me/plugins`.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::orchestrator::DelegateRun;
use crate::models::specialist::Specialist;
use crate::specialists::registry;
use crate::storage::apps::AppIdentity;
use crate::storage::{sessions, specialists as store};

use super::AppState;

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

/// `GET /api/specialists`
pub async fn list(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    match store::list_visible(&state.db, &identity.app_id).await {
        Ok(rows) => Json(json!({"specialists": rows})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
pub struct WriteRequest {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub tool_allow: Vec<String>,
    #[serde(default)]
    pub response_schema: Option<Value>,
    #[serde(default)]
    pub max_steps: Option<i64>,
    /// Absolute reasoning-token cap for a run. Wins over `reasoning_cap_fraction`
    /// when both are given, matching the configured-beats-derived rule used
    /// throughout the provider layer.
    #[serde(default)]
    pub reasoning_cap_tokens: Option<i32>,
    /// Share of the delegate's remaining context window it may spend thinking.
    /// Omit both to inherit the daemon default.
    #[serde(default)]
    pub reasoning_cap_fraction: Option<f64>,
    /// `"per_ref"` to split a call with N refs into N delegates, or omit for a
    /// single run over all of them.
    #[serde(default)]
    pub fan_out: Option<String>,
    #[serde(default)]
    pub max_concurrent: Option<i64>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// `POST /api/specialists`
///
/// Also the edit path: writing a name that already exists for this app replaces
/// that definition, and writing a built-in's name creates the app's shadow of
/// it. One route for both because "edit the researcher" and "define my own
/// researcher" are the same operation from the daemon's side.
pub async fn create(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Json(body): Json<WriteRequest>,
) -> Response {
    let name = body.name.trim();
    if name.is_empty() || body.description.trim().is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "name and description are required; the description is what the model routes on",
        );
    }

    // An unknown tool name fails here rather than at run time. A typo is
    // otherwise invisible until a delegate quietly does its job without the one
    // tool that mattered — and neither the model nor the user can tell that
    // apart from the tool simply not having helped.
    let unknown = registry::unknown_tools(&state.mcp, &identity.app_id, &body.tool_allow);
    if !unknown.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            format!("no connected server provides: {}", unknown.join(", ")),
        );
    }

    // Preserve the id across an edit so a client's links and any UI selection
    // survive; a new name gets a new id.
    let id = match store::resolve(&state.db, &identity.app_id, name).await {
        Ok(Some(existing)) if existing.app_id.as_deref() == Some(identity.app_id.as_str()) => {
            existing.id
        }
        Ok(_) => uuid::Uuid::new_v4().to_string(),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let spec = Specialist {
        id: id.clone(),
        app_id: Some(identity.app_id.clone()),
        name: name.to_string(),
        description: body.description.trim().to_string(),
        system_prompt: body.system_prompt,
        provider: body.provider,
        model: body.model,
        tool_allow: body.tool_allow,
        response_schema: body.response_schema,
        max_steps: body.max_steps.unwrap_or(20).clamp(1, 200),
        reasoning_cap: match (body.reasoning_cap_tokens, body.reasoning_cap_fraction) {
            (Some(n), _) => Some(crate::agent::tokens::ReasoningCap::Tokens(n.max(0))),
            (None, Some(f)) => Some(crate::agent::tokens::ReasoningCap::ContextFraction(
                f.clamp(0.0, 1.0),
            )),
            (None, None) => None,
        },
        // Only the one shape is understood; anything else is stored as "no
        // fan-out" rather than silently behaving like `per_ref`.
        fan_out: body
            .fan_out
            .filter(|f| f == "per_ref"),
        max_concurrent: body.max_concurrent,
        enabled: body.enabled,
        // Never settable over the wire: `builtin` is what makes a row
        // undeletable and shared, and an app declaring itself one would be
        // writing a definition every other app then runs.
        builtin: false,
    };

    match store::upsert(&state.db, &spec).await {
        Ok(()) => Json(json!({"id": id})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `DELETE /api/specialists/{id}`
///
/// On an app's own row this deletes it — reverting to the built-in of the same
/// name, if there is one. On a built-in it is a 403: shared definitions are not
/// one app's to remove.
pub async fn delete(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(id): Path<String>,
) -> Response {
    match store::get_any(&state.db, &id).await {
        Ok(Some(spec)) if spec.app_id.is_none() => {
            return err(
                StatusCode::FORBIDDEN,
                "built-in specialists cannot be deleted; define one with the same name to \
                 override it for this app",
            )
        }
        Ok(Some(_)) => {}
        Ok(None) => return err(StatusCode::NOT_FOUND, format!("no such specialist: {id}")),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }

    match store::delete_for_app(&state.db, &id, &identity.app_id).await {
        // Someone else's. Reported as absent, not forbidden, per the daemon's
        // 404-not-403 rule for rows an app does not own.
        Ok(0) => err(StatusCode::NOT_FOUND, format!("no such specialist: {id}")),
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
pub struct RunRequest {
    pub request: String,
    #[serde(default)]
    pub refs: Option<Vec<String>>,
    /// The session this run belongs under. Required, because a delegate is
    /// always a delegate *of* something — see the `parent_session_id` tag.
    pub session_id: String,
}

/// `POST /api/specialists/{name}/run`
///
/// The app-driven entry point, for a UI action or a scheduled task, alongside
/// the model-driven one in `specialists::server`. Both go through the same
/// orchestrator, so the concurrency cap, depth limit and cancel propagation
/// apply identically — there is no second way to start a delegate.
pub async fn run(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(name): Path<String>,
    Json(body): Json<RunRequest>,
) -> Response {
    if body.request.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "request must not be empty");
    }
    // Checked before anything runs: a parent in another app would let one app
    // graft its delegates onto another's tree and bill them for it.
    match sessions::is_owned_by(&state.db, &body.session_id, &identity.app_id).await {
        Ok(true) => {}
        Ok(false) => {
            return err(
                StatusCode::NOT_FOUND,
                format!("no such session: {}", body.session_id),
            )
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }

    let spec = match store::resolve(&state.db, &identity.app_id, &name).await {
        Ok(Some(s)) if s.enabled => s,
        Ok(Some(_)) => return err(StatusCode::NOT_FOUND, format!("{name} is disabled")),
        Ok(None) => return err(StatusCode::NOT_FOUND, format!("no such specialist: {name}")),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let mut prompt = body.request.trim().to_string();
    if let Some(refs) = body.refs.as_ref().filter(|r| !r.is_empty()) {
        prompt.push_str("\n\nWork from these sources:\n");
        for r in refs {
            prompt.push_str(&format!("- {r}\n"));
        }
    }

    let run = DelegateRun {
        name: spec.name.clone(),
        parent_session_id: body.session_id,
        prompt,
        system_prompt: Some(spec.system_prompt),
        provider: spec.provider,
        model: spec.model,
        tool_allow: spec
            .tool_allow
            .into_iter()
            .filter(|t| {
                !crate::specialists::server::SESSION_SCOPED_TOOLS.contains(&t.as_str())
            })
            .collect(),
        response_schema: spec.response_schema,
        max_steps: spec.max_steps,
        reasoning_cap: spec.reasoning_cap,
    };

    match state.orchestrator.run(run).await {
        Ok(Ok(outcome)) => {
            let result = serde_json::from_str::<Value>(&outcome.answer)
                .unwrap_or_else(|_| json!(outcome.answer));
            Json(json!({
                "ok": true,
                "ran_on": outcome.host,
                "notes": outcome.notes,
                "result": result,
            }))
            .into_response()
        }
        // The run happened and failed, which is not a client error: the request
        // was well-formed and the daemon accepted it.
        Ok(Err(why)) => err(StatusCode::INTERNAL_SERVER_ERROR, why),
        Err(refusal) => err(StatusCode::CONFLICT, refusal.to_string()),
    }
}

/// `GET /api/specialists/runs`
///
/// What was delegated, to whom, and how it went.
///
/// `execution_history` has recorded `trigger_type = 'subagent'` rows since the
/// orchestrator was written and nothing has ever read them. This is the only way
/// a bad `description` becomes diagnosable — routing depends entirely on that
/// field, and a vague one does not fail, it just gets used for the wrong things.
/// Seeing which specialist actually answered which request is what makes that
/// visible.
pub async fn runs(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    let rows = sqlx::query_as::<_, (String, Option<String>, Option<String>, String, Option<String>, Option<String>)>(
        "SELECT e.id, e.trigger_id, e.session_id, e.status, e.started_at, e.result_summary          FROM execution_history e          JOIN sessions s ON s.id = e.session_id          WHERE e.trigger_type = 'subagent' AND s.app_id = ?          ORDER BY e.started_at DESC LIMIT 100",
    )
    .bind(&identity.app_id)
    .fetch_all(&state.db)
    .await;

    match rows {
        Ok(rows) => {
            let runs: Vec<Value> = rows
                .into_iter()
                .map(|(id, specialist, session_id, status, started_at, summary)| {
                    json!({
                        "id": id,
                        "specialist": specialist,
                        // The delegate's own session, so a reader can open the
                        // transcript when the summary was not enough.
                        "session_id": session_id,
                        "status": status,
                        "started_at": started_at,
                        "summary": summary,
                    })
                })
                .collect();
            Json(json!({"runs": runs})).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
