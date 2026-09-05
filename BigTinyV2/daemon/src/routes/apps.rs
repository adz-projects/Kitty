//! `/api/apps` — registration, defaults, revocation.
//!
//! The route family that has no V1 equivalent, because V1 had no concept of a
//! client. An app registers once with the bootstrap token from the handshake,
//! stores the returned key in its own secret store, and thereafter identifies
//! itself with `X-API-Key`.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use bigtiny2_protocol::discovery::{RegisterRequest, RegisterResponse};
use serde::Deserialize;
use serde_json::json;

use crate::discovery::generate_token;
use crate::storage::apps::{self, AppIdentity};

use super::AppState;

fn err_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
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
        // make the whole per-app boundary meaningless.
        Ok(None) => err_response(
            StatusCode::CONFLICT,
            format!("app id {app_id:?} is already registered"),
        ),
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
