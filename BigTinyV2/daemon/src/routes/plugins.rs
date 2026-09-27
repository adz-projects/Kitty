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

use crate::storage::app_plugins::{self, MEMORABILIA, PATHWAY};
use crate::storage::apps::AppIdentity;

use super::AppState;

/// Plugins this daemon knows how to host. A name outside this list is a
/// client typo, and answering 404 says so rather than silently storing a
/// preference nothing will ever read.
const KNOWN_PLUGINS: [&str; 2] = [PATHWAY, MEMORABILIA];

/// Whether `plugin` is effectively enabled for `app_id`, dispatched to the
/// host that owns it.
async fn plugin_enabled(state: &AppState, plugin: &str, app_id: &str) -> bool {
    match plugin {
        MEMORABILIA => state.memorabilia.is_enabled(app_id).await,
        _ => state.plugins.is_enabled(app_id).await,
    }
}

/// Close `plugin`'s live per-app instance (stops its background work), on the
/// host that owns it.
async fn plugin_close(state: &AppState, plugin: &str, app_id: &str) {
    match plugin {
        MEMORABILIA => state.memorabilia.close(app_id).await,
        _ => state.plugins.close(app_id).await,
    }
}

/// Re-establish `plugin`'s in-process MCP server for `app_id` after its
/// engine was opened, closed or erased.
///
/// The `pathway`/`memorabilia` tool servers take their engine once, at
/// connect time, and hold on to it. Without this, a disabled engine's
/// `record`/`forget`/`memorabilia_search` kept working against the instance
/// that was open when the server connected, and a freshly enabled one stayed
/// without tools until the health watcher happened to retry. The server is
/// reconnected only if the app wants it and the plugin is now on; otherwise it
/// is left disconnected.
pub(crate) async fn reconnect_plugin_tools(state: &AppState, plugin: &str, app_id: &str) {
    let enabled = plugin_enabled(state, plugin, app_id).await;
    for row in plugin_tool_rows(state, plugin, app_id).await {
        state.mcp.disconnect_server(&row.id).await;
        if enabled && row.enabled != 0 {
            if let Err(e) = state.mcp.connect_server(&row.id).await {
                tracing::warn!("could not reconnect the {plugin} tools for {app_id}: {e}");
            }
        }
    }
}

/// The app's in-process tool-server rows for `plugin`.
async fn plugin_tool_rows(
    state: &AppState,
    plugin: &str,
    app_id: &str,
) -> Vec<crate::storage::mcp_servers::MCPServerRow> {
    match crate::storage::mcp_servers::list_servers_for_app(&state.db, app_id).await {
        Ok(rows) => rows
            .into_iter()
            .filter(|r| {
                r.transport == "in_process"
                    && r.command.as_deref() == Some(plugin)
                    && r.app_id.as_deref() == Some(app_id)
            })
            .collect(),
        Err(e) => {
            tracing::warn!("could not list MCP servers for the {plugin} tools: {e}");
            Vec::new()
        }
    }
}

/// Install a V1 belief graph as `app_id`'s, if the app has none of its own.
///
/// Never merges: two graphs' beliefs, evidence and decay state do not
/// combine meaningfully, and the user's current graph is the one that is
/// being used. So the V1 file is adopted only when the app's `pathway.db` is
/// missing or holds no beliefs, and reported as skipped otherwise.
///
/// The app's engine and its tool server are closed first so the file is not
/// in use, and reconnected afterwards (which reopens the engine on the new
/// file). If something still holds the file - a turn mid-recall - the old
/// file cannot be removed on Windows and the import reports `skipped_in_use`
/// rather than writing over an open database.
pub(crate) async fn adopt_v1_pathway(
    state: &AppState,
    app_id: &str,
    source: &std::path::Path,
) -> &'static str {
    if !source.is_file() {
        return "not_found";
    }
    let dest = state.plugins.db_path(app_id);

    for row in plugin_tool_rows(state, PATHWAY, app_id).await {
        state.mcp.disconnect_server(&row.id).await;
    }
    state.plugins.close(app_id).await;

    let outcome = replace_if_empty(source, &dest).await;
    reconnect_plugin_tools(state, PATHWAY, app_id).await;
    outcome
}

async fn replace_if_empty(source: &std::path::Path, dest: &std::path::Path) -> &'static str {
    if dest.exists() {
        match belief_count(dest).await {
            Some(0) => {}
            // Unreadable counts as not empty: never replace what we cannot see.
            _ => return "skipped_not_empty",
        }
        for suffix in ["", "-wal", "-shm"] {
            let path = std::path::PathBuf::from(format!("{}{suffix}", dest.display()));
            if path.exists() && std::fs::remove_file(&path).is_err() {
                return "skipped_in_use";
            }
        }
    } else if let Some(parent) = dest.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return "failed";
        }
    }
    let copied = std::fs::copy(source, dest).is_ok()
        && ["-wal", "-shm"].into_iter().all(|suffix| {
            let from = std::path::PathBuf::from(format!("{}{suffix}", source.display()));
            !from.exists()
                || std::fs::copy(&from, format!("{}{suffix}", dest.display())).is_ok()
        });
    if copied {
        "imported"
    } else {
        "failed"
    }
}

/// How many beliefs the graph at `path` holds, read without disturbing it.
async fn belief_count(path: &std::path::Path) -> Option<i64> {
    use sqlx::ConnectOptions;
    let mut conn = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .connect()
        .await
        .ok()?;
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM beliefs")
        .fetch_one(&mut conn)
        .await
        .ok();
    let _ = sqlx::Connection::close(conn).await;
    count
}

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
            "enabled": plugin_enabled(&state, name, &identity.app_id).await,
            // Distinguishes "I chose this" from "I inherited the default",
            // which matters because a daemon-default change moves the latter
            // and not the former.
            "explicit": explicit.is_some(),
        }));
    }
    Json(json!({
        "plugins": out,
        // Whether this daemon loaded a semantic embedding model. Without one
        // both memory engines fall back to lexical hashing; a client that
        // treats memory as needing real embeddings (Kitty does) can keep them
        // off instead, and knows a restart is needed once a model is added.
        "semantic_embeddings": state.plugins.embedder().is_some(),
    }))
    .into_response()
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
        plugin_close(&state, &plugin, &identity.app_id).await;
    }
    // Either way the tool server must follow: gone when off, connected (to a
    // freshly opened engine) when on.
    reconnect_plugin_tools(&state, &plugin, &identity.app_id).await;

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
            plugin_close(&state, &plugin, &identity.app_id).await;
            reconnect_plugin_tools(&state, &plugin, &identity.app_id).await;
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
