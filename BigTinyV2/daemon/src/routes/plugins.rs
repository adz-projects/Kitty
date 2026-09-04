//! `/api/apps/me/plugins` — each app's own plugin selection.
//!
//! Deliberately separate from `/api/mcp/servers`. A *plugin* hooks the agent
//! loop and carries per-app instance state; an *MCP server* provides tools
//! across the MCP boundary and is selected by an `app_id` column. Folding them
//! into one endpoint would re-create exactly the conflation `crate::plugins`
//! exists to remove.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::json;

use crate::storage::app_plugins::{self, PATHWAY};
use crate::storage::apps::AppIdentity;

use super::AppState;

/// Plugins this daemon knows how to host. A name outside this list is a
/// client typo, and answering 404 says so rather than silently storing a
/// preference nothing will ever read.
const KNOWN_PLUGINS: [&str; 1] = [PATHWAY];

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// `GET /api/apps/me/plugins`
///
/// Reports the *effective* state, not just stored rows: a plugin the app has
/// never expressed a preference about still reports whether it is on, because
/// "what will actually happen on my next turn" is the question a caller is
/// asking.
pub async fn list(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    let stored = match app_plugins::list_for_app(&state.db, &identity.app_id).await {
        Ok(rows) => rows,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let mut out = Vec::new();
    for name in KNOWN_PLUGINS {
        let explicit = stored.iter().find(|r| r.plugin == name);
        out.push(json!({
            "plugin": name,
            "enabled": state.plugins.is_enabled(&identity.app_id).await,
            // Distinguishes "I chose this" from "I inherited the default",
            // which matters because a daemon-default change moves the latter
            // and not the former.
            "explicit": explicit.is_some(),
        }));
    }
    Json(json!({ "plugins": out })).into_response()
}

#[derive(Debug, Deserialize)]
pub struct SetPluginRequest {
    pub enabled: bool,
}

/// `PUT /api/apps/me/plugins/{plugin}`
pub async fn set(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(plugin): Path<String>,
    Json(body): Json<SetPluginRequest>,
) -> Response {
    if !KNOWN_PLUGINS.contains(&plugin.as_str()) {
        return err(StatusCode::NOT_FOUND, format!("unknown plugin: {plugin}"));
    }

    if let Err(e) = app_plugins::set_enabled(&state.db, &identity.app_id, &plugin, body.enabled).await
    {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    // Turning a plugin off must actually stop it, not merely stop new turns
    // from consulting it: an open engine keeps a background sweep running and
    // a SQLite file held. `pathway_for` short-circuits on an already-open
    // instance, so without this the plugin would stay live until restart.
    if !body.enabled {
        state.plugins.close(&identity.app_id).await;
    }

    Json(json!({"ok": true})).into_response()
}

/// `DELETE /api/apps/me/plugins/{plugin}` — return to the daemon default.
pub async fn clear(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(plugin): Path<String>,
) -> Response {
    if !KNOWN_PLUGINS.contains(&plugin.as_str()) {
        return err(StatusCode::NOT_FOUND, format!("unknown plugin: {plugin}"));
    }
    match app_plugins::clear(&state.db, &identity.app_id, &plugin).await {
        Ok(_) => {
            // The default may be "off", so drop any live instance and let the
            // next turn re-decide from scratch.
            state.plugins.close(&identity.app_id).await;
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
