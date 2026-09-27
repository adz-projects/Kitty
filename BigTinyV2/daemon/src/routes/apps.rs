//! `/api/apps` — registration, defaults, revocation.
//!
//! The route family that has no V1 equivalent, because V1 had no concept of a
//! client. An app registers once with the bootstrap token from the handshake,
//! stores the returned key in its own secret store, and thereafter identifies
//! itself with `X-API-Key`.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use bigtiny2_protocol::discovery::{ReclaimRequest, RegisterRequest, RegisterResponse};
use serde::Deserialize;
use serde_json::json;

use crate::discovery::generate_token;
use crate::storage::apps::{self, AppIdentity};

use super::AppState;

fn err_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

/// Body of `POST /api/apps/me/import-v1`.
#[derive(Debug, Deserialize)]
pub struct ImportV1Request {
    /// The V1 daemon's `bigtiny.db`. Read from a copy; never modified.
    pub v1_db_path: String,
    /// The key V1 encrypted its secrets with, hex-encoded.
    pub v1_encryption_key_hex: String,
    /// V1's belief graph, adopted only if this app has none of its own.
    #[serde(default)]
    pub pathway_db_path: Option<String>,
}

/// `POST /api/apps/me/import-v1`
///
/// Merge a Kitty V1 database into the calling app's data in the live
/// database: sessions and their history, providers (with their keys moved
/// onto this daemon's key), MCP servers, and approval rules. Anything already
/// present is skipped, not overwritten, so a repeated import is harmless. See
/// `import::merge_v1` for the details and `routes::plugins::adopt_v1_pathway`
/// for the belief graph.
///
/// Answers with an `import::MergeSummary`. New providers are registered with
/// the router and new MCP servers connected before or just after it returns,
/// so they are usable without a restart.
pub async fn import_v1(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Json(body): Json<ImportV1Request>,
) -> Response {
    // One at a time: two concurrent merges of the same source would each see
    // the other's rows as absent until commit.
    static RUNNING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let Ok(_running) = RUNNING.try_lock() else {
        return err_response(StatusCode::CONFLICT, "an import is already running");
    };

    let key = match crate::crypto::parse_key_hex(&body.v1_encryption_key_hex) {
        Ok(key) => key,
        Err(e) => {
            return err_response(
                StatusCode::BAD_REQUEST,
                format!("v1_encryption_key_hex: {e}"),
            )
        }
    };
    let source = PathBuf::from(&body.v1_db_path);
    if !source.is_file() {
        return err_response(
            StatusCode::BAD_REQUEST,
            format!("no V1 database at {}", source.display()),
        );
    }
    if is_live_database(&state.db, &source).await {
        return err_response(
            StatusCode::BAD_REQUEST,
            "that is this daemon's own database, not a V1 one",
        );
    }

    let app_id = identity.app_id.as_str();
    let outcome = match crate::import::merge_v1(
        &state.db,
        state.plugins.data_dir(),
        &source,
        &key,
        app_id,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    for id in &outcome.new_provider_ids {
        match crate::storage::providers::get_provider(&state.db, id).await {
            Ok(Some(row)) => state.router.register_from_row(&row),
            Ok(None) => {}
            Err(e) => tracing::warn!("imported provider {id} could not be registered: {e}"),
        }
    }
    // Connecting can take up to the connect timeout per server; the import
    // itself is done, so do not hold the answer for it.
    let to_connect = outcome.new_mcp_server_ids.clone();
    let mcp = state.mcp.clone();
    let db = state.db.clone();
    tokio::spawn(async move {
        for id in to_connect {
            let enabled = matches!(
                crate::storage::mcp_servers::get_server(&db, &id).await,
                Ok(Some(row)) if row.enabled != 0
            );
            if enabled {
                if let Err(e) = mcp.connect_server(&id).await {
                    tracing::warn!("imported MCP server {id} did not connect: {e}");
                }
            }
        }
    });

    let mut summary = outcome.summary;
    summary.pathway = match body.pathway_db_path.as_deref() {
        None => "not_requested",
        Some(path) => {
            super::plugins::adopt_v1_pathway(&state, app_id, std::path::Path::new(path)).await
        }
    }
    .to_string();
    Json(summary).into_response()
}

/// Query of `DELETE /api/apps/me`.
#[derive(Debug, Default, Deserialize)]
pub struct DeleteMeQuery {
    /// Delete everything the app owns as well, instead of only revoking it.
    #[serde(default)]
    pub purge: bool,
}

/// `DELETE /api/apps/me[?purge=true]`
///
/// Without `purge`, the same revocation as `DELETE /api/apps/{own id}`: the
/// key stops working and the app's data stays, recoverable by registering
/// the same id again.
///
/// With `purge`, the app is removed for good - for an uninstall that asked to
/// delete its data. Every row the app owns goes (sessions and their history,
/// providers and their keys, MCP servers, schedules, specialists, approval
/// rules, plugin choices), then its directory under the data dir (belief
/// graph, memory, plugin home, grants), then the app itself. Other apps' data
/// and anything shared with every app are untouched.
///
/// Refused with 409 while the app has a turn running: deleting a session out
/// from under its own turn would leave that turn writing to rows that are
/// gone. The answer names the counts removed, and `files_removed: false` if
/// the directory could not be deleted (a file still in use); the rows and the
/// app are gone either way.
pub async fn delete_me(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Query(query): Query<DeleteMeQuery>,
) -> Response {
    let app_id = identity.app_id.clone();
    if !query.purge {
        return delete(State(state), Extension(identity), Path(app_id)).await;
    }

    for session in state.agent.active_session_ids() {
        if matches!(
            crate::storage::sessions::owner_of(&state.db, &session).await,
            Ok(Some(owner)) if owner == app_id
        ) {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "a turn is still running; stop it and try again",
                    "reason": "active_turn",
                    "session_id": session,
                })),
            )
                .into_response();
        }
    }

    // Take down what is live before the rows it was built from disappear.
    let owned = |table: &'static str| {
        let db = state.db.clone();
        let app_id = app_id.clone();
        async move {
            sqlx::query_scalar::<_, String>(&format!("SELECT id FROM {table} WHERE app_id = ?"))
                .bind(app_id)
                .fetch_all(&db)
                .await
                .unwrap_or_default()
        }
    };
    for id in owned("schedule_jobs").await {
        if let Err(e) = state.scheduler.lock().await.remove_job(&id).await {
            tracing::warn!("purge: could not stop schedule {id}: {e}");
        }
    }
    for id in owned("mcp_servers").await {
        state.mcp.disconnect_server(&id).await;
    }
    let providers = owned("providers").await;
    state.plugins.close(&app_id).await;
    state.memorabilia.close(&app_id).await;

    let counts = match apps::purge_app(&state.db, &app_id).await {
        Ok(counts) => counts,
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    for id in &providers {
        state.router.unregister(id);
    }
    // Synchronously, before responding, as for a plain revocation.
    state.key_cache.invalidate(&app_id);

    let dir = state.plugins.data_dir().join("apps").join(&app_id);
    let files_removed = remove_dir_with_retry(&dir).await;
    Json(json!({
        "ok": true,
        "purged": counts,
        "files_removed": files_removed,
    }))
    .into_response()
}

/// Remove `dir`, retrying briefly for files that are closed but not yet
/// released (Windows). `true` when it is gone.
async fn remove_dir_with_retry(dir: &std::path::Path) -> bool {
    for attempt in 0..20 {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
            Err(e) if attempt == 19 => {
                tracing::warn!("purge: could not remove {}: {e}", dir.display());
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
    false
}

/// Whether `path` is the file behind `pool`'s main database.
async fn is_live_database(pool: &sqlx::SqlitePool, path: &std::path::Path) -> bool {
    let live: Option<String> =
        sqlx::query_scalar("SELECT file FROM pragma_database_list WHERE name = 'main'")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    let Some(live) = live.filter(|f| !f.is_empty()) else {
        return false;
    };
    match (std::fs::canonicalize(&live), std::fs::canonicalize(path)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// `POST /api/apps/register`
///
/// Gated on `X-Registration-Token` by the auth middleware, not on an app key —
/// the caller has no key yet, which is why it is here.
///
/// The generated key is returned **once** and never stored in plaintext (only
/// its SHA-256 hash), so there is no recovery path for an app that loses it
/// beyond registering a fresh id or having the user revoke the old one. That is
/// the intended trade: a leaked database yields no usable credentials.
pub async fn register(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RegisterRequest>,
) -> Response {
    let app_id = body.app_id.trim();
    if app_id.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, "app_id must not be empty");
    }
    // Reserved because `app_id` is stamped into `sessions.app_id`, whose
    // migration default is `''` for rows that must be unreachable. An app
    // literally named "" would make those rows addressable.
    if app_id.len() > 64 {
        return err_response(StatusCode::BAD_REQUEST, "app_id must be 64 chars or fewer");
    }
    // An app id becomes a path segment: `PluginHost` opens
    // `<data_dir>/apps/<app_id>/pathway.db` and `scoped_env` derives that
    // app's `KITTY_PLUGIN_HOME` the same way. Without a charset rule, `..`
    // walks out of the data dir, and a separator or a NUL puts the database
    // somewhere nobody intended. Restrict it to what is safe as both a
    // directory name and an identifier on every platform the daemon runs on.
    let shaped = app_id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
        && app_id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && app_id != "."
        && app_id != ".."
        && !app_id.contains("..");
    if !shaped {
        return err_response(
            StatusCode::BAD_REQUEST,
            "app_id must start with a lowercase letter or digit and contain only              lowercase letters, digits, '-', '_' and '.'",
        );
    }

    let api_key = generate_token();
    match apps::register_app(&state.db, app_id, &body.display_name, &api_key).await {
        Ok(Some(key)) => Json(RegisterResponse {
            app_id: app_id.to_string(),
            api_key: key,
        })
        .into_response(),
        // Already registered. A 409 rather than silently reissuing a key:
        // reissuing would let anyone holding the (per-launch, file-readable)
        // registration token take over an existing app's identity, which would
        // make the whole per-app boundary meaningless. An app that genuinely
        // lost its key uses `reclaim`, which refuses while the key is in use
        // and revokes the old key rather than issuing a second one.
        Ok(None) => err_response(
            StatusCode::CONFLICT,
            format!("app id {app_id:?} is already registered"),
        ),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// How recently an app must *not* have been seen for its identity to be
/// reclaimable. `last_seen_at` is written about once a minute, so this is a
/// little over one write interval.
const RECLAIM_QUIET_SECS: u64 = 90;

/// `POST /api/apps/reclaim` — re-issue the key of an app that lost it.
///
/// Registration refuses to reissue an existing app's key (see `register`),
/// which left an app whose stored key was lost with no way back: it could not
/// authenticate, could not register again, and could not revoke itself. This
/// is that way back. It is gated on the same registration token as `register`
/// -- proof of read access to this daemon's data directory, which on a
/// single-user machine is the same trust boundary the app's own secret store
/// sits behind.
///
/// Two limits keep it from being a quiet takeover path:
/// * an app that authenticated within [`RECLAIM_QUIET_SECS`] cannot be
///   reclaimed (409) -- something is actively using that key;
/// * the old key stops working immediately, so a reclaim is never silent to
///   the key's holder: its next request fails.
pub async fn reclaim(State(state): State<Arc<AppState>>, Json(body): Json<ReclaimRequest>) -> Response {
    let app_id = body.app_id.trim();
    match apps::get_app(&state.db, app_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return err_response(StatusCode::NOT_FOUND, format!("no such app: {app_id}")),
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
    match apps::seen_within_secs(&state.db, app_id, RECLAIM_QUIET_SECS).await {
        Ok(false) => {}
        Ok(true) => {
            return err_response(
                StatusCode::CONFLICT,
                format!(
                    "app {app_id:?} is in use; it can be reclaimed once it has been idle                      for {RECLAIM_QUIET_SECS} seconds"
                ),
            )
        }
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
    let api_key = generate_token();
    match apps::replace_key(&state.db, app_id, &api_key).await {
        Ok(true) => {
            // Synchronously, before responding, same as revocation: the old
            // key must not authenticate once the new one exists.
            state.key_cache.invalidate(app_id);
            tracing::warn!("app {app_id:?} reclaimed its identity with a new key");
            Json(RegisterResponse {
                app_id: app_id.to_string(),
                api_key,
            })
            .into_response()
        }
        Ok(false) => err_response(StatusCode::NOT_FOUND, format!("no such app: {app_id}")),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `GET /api/apps/me` — the calling app's own record.
pub async fn get_me(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    match apps::get_app(&state.db, &identity.app_id).await {
        Ok(Some(app)) => Json(json!({
            "app_id": app.id,
            "display_name": app.display_name,
            "scopes": app.scopes,
            "default_provider_id": app.default_provider_id,
            "default_model": app.default_model,
            "created_at": app.created_at,
            "last_seen_at": app.last_seen_at,
        }))
        .into_response(),
        // Authenticated but absent means the app was revoked between the auth
        // cache being primed and this call. Treat as unauthorized, not 404.
        Ok(None) => err_response(StatusCode::UNAUTHORIZED, "app no longer registered"),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateMeRequest {
    /// Explicit `null` clears the default; an absent key leaves it unchanged.
    /// `Option<Option<T>>` is what distinguishes those two, and the
    /// distinction matters: "unset my default" and "don't touch my default"
    /// are different requests.
    #[serde(default, deserialize_with = "double_option")]
    pub default_provider_id: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub default_model: Option<Option<String>>,
}

fn double_option<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    serde::Deserialize::deserialize(de).map(Some)
}

/// `PATCH /api/apps/me` — set this app's default provider/model.
///
/// **This is what replaces V1's `demote_others`.** In V1 the active provider
/// was a daemon-global sort over `fallback_priority`, so Kitty expressed "use
/// mine" by PATCHing priority 100 onto every row it did not own
/// (`src-tauri/src/bigtiny/providers.rs:222`) — a write that would silently
/// clobber every other app's choice the moment a second app existed. Here the
/// preference is a column on the caller's own row, and cannot reach anyone
/// else's.
pub async fn update_me(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Json(body): Json<UpdateMeRequest>,
) -> Response {
    let current = match apps::get_app(&state.db, &identity.app_id).await {
        Ok(Some(app)) => app,
        Ok(None) => return err_response(StatusCode::UNAUTHORIZED, "app no longer registered"),
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let provider = match body.default_provider_id {
        Some(v) => v,
        None => current.default_provider_id,
    };
    let model = match body.default_model {
        Some(v) => v,
        None => current.default_model,
    };

    match apps::set_app_default(
        &state.db,
        &identity.app_id,
        provider.as_deref(),
        model.as_deref(),
    )
    .await
    {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `GET /api/apps` — every registered app.
///
/// Deliberately readable by any registered app and deliberately free of
/// secrets: knowing that a notebook is also connected is useful for
/// diagnostics ("who else is holding this provider's slots?"), and the row
/// carries no key material to leak.
pub async fn list(State(state): State<Arc<AppState>>) -> Response {
    match apps::list_apps(&state.db).await {
        Ok(rows) => Json(json!({
            "apps": rows.iter().map(|a| json!({
                "app_id": a.id,
                "display_name": a.display_name,
                "created_at": a.created_at,
                "last_seen_at": a.last_seen_at,
            })).collect::<Vec<_>>()
        }))
        .into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `DELETE /api/apps/{id}` — revoke.
///
/// The app's rows are left in place (see `storage::apps::delete_app`), so a
/// mistyped revocation is recoverable by re-registering the same id rather
/// than being an irreversible data loss.
pub async fn delete(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(target): Path<String>,
) -> Response {
    // An app may revoke only itself. Cross-app revocation would hand any
    // registered client a denial-of-service against every other one.
    if target != identity.app_id {
        return err_response(StatusCode::FORBIDDEN, "an app may only revoke itself");
    }

    match apps::delete_app(&state.db, &target).await {
        Ok(0) => err_response(StatusCode::NOT_FOUND, format!("no such app: {target}")),
        Ok(_) => {
            // Synchronously, before responding: the caller must not be able to
            // make another authenticated request with the key it just revoked.
            state.key_cache.invalidate(&target);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
