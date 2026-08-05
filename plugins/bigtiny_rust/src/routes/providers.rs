//! `/api/providers` routes — mirrors
//! `plugins/bigtiny/bigtiny/server/routes/providers.py`. The `providers`
//! table has no dedicated `api_key` column (see migration 001) — Python
//! stores it inside the `config` JSON blob, and so does this port; a
//! top-level `api_key` in the request body gets folded into `config` before
//! persisting.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::crypto;
use crate::storage::providers;

use super::AppState;

fn err_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

/// Merge a request body's `config`/`api_key` fields onto `existing_config`
/// (the provider's currently-stored config JSON, `None` for a brand new
/// row). The `providers` table has no dedicated `api_key` column (see
/// migration 001) — it lives inside this JSON blob — so this must start
/// from what's already stored, not an empty object: a PATCH that only
/// touches, say, `fallback_priority` (exactly what Kitty's own
/// `sync_active_provider` demotion loop sends) must never wipe an
/// already-configured API key or model just because this request didn't
/// happen to repeat it.
fn merge_config(existing_config: Option<&str>, body: &Value) -> String {
    let mut config: Value = existing_config
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| json!({}));

    if let Some(patch) = body.get("config").and_then(|v| v.as_object()) {
        if let Some(obj) = config.as_object_mut() {
            for (k, v) in patch {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
    // A field entirely absent from `body` means "don't touch the stored
    // key" (see the demotion-loop case above). An explicit JSON `null` is
    // different: it means "clear it" — Kitty sends that when the key was
    // removed from Windows Credential Manager. Without this distinction,
    // that removal could never actually reach BigTiny's stored config (an
    // omitted field and an explicit clear both looked like "no change"),
    // so a deleted key kept working here forever.
    match body.get("api_key") {
        Some(Value::Null) => {
            if let Some(obj) = config.as_object_mut() {
                obj.remove("api_key");
            }
        }
        Some(Value::String(key)) => {
            if let Some(obj) = config.as_object_mut() {
                // Encrypted before it's ever written — the "field omitted"
                // pass-through path above needs no change: it leaves
                // whatever's already stored (encrypted or legacy plaintext)
                // untouched, and `decrypt` handles either transparently.
                obj.insert("api_key".to_string(), json!(crypto::encrypt(key)));
            }
        }
        _ => {}
    }
    config.to_string()
}

/// Redact a provider row for an HTTP response: never echo the (encrypted or
/// legacy-plaintext) `api_key` value back over the wire — `has_api_key`
/// tells the caller whether one is configured without exposing it, matching
/// how Kitty's own Settings UI already only ever shows "🔑 key stored"
/// rather than the real value.
fn to_public_json(row: &providers::ProviderRow) -> Value {
    let mut json = serde_json::to_value(row).unwrap_or_else(|_| json!({}));
    let mut has_api_key = false;
    if let Some(config_str) = row.config.as_deref() {
        if let Ok(mut config_val) = serde_json::from_str::<Value>(config_str) {
            if let Some(obj) = config_val.as_object_mut() {
                if let Some(Value::String(key)) = obj.remove("api_key") {
                    has_api_key = !key.is_empty();
                }
            }
            json["config"] = Value::String(config_val.to_string());
        }
    }
    if let Some(obj) = json.as_object_mut() {
        obj.insert("has_api_key".to_string(), json!(has_api_key));
    }
    json
}

pub async fn list_providers(State(state): State<Arc<AppState>>) -> Response {
    match providers::list_providers(&state.db).await {
        Ok(rows) => {
            let redacted: Vec<Value> = rows.iter().map(to_public_json).collect();
            Json(json!({"providers": redacted})).into_response()
        }
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn create_provider(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let (Some(name), Some(provider_type), Some(base_url)) = (
        body.get("name").and_then(|v| v.as_str()),
        body.get("provider_type").and_then(|v| v.as_str()),
        body.get("base_url").and_then(|v| v.as_str()),
    ) else {
        return err_response(
            StatusCode::BAD_REQUEST,
            "name, provider_type, base_url are required",
        );
    };

    let id = uuid::Uuid::new_v4().to_string();
    let row = match providers::create_provider(&state.db, &id, name, provider_type, base_url).await
    {
        Ok(r) => r,
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let config = merge_config(None, &body);
    let fallback_priority = body.get("fallback_priority").and_then(|v| v.as_i64());
    if let Err(e) = sqlx::query(
        r#"UPDATE providers SET config = ?1, fallback_priority = COALESCE(?2, fallback_priority) WHERE id = ?3"#
    )
    .bind(&config)
    .bind(fallback_priority.map(|v| v as i32))
    .bind(&id)
    .execute(&state.db)
    .await
    {
        // Was `let _ = ...` — a swallowed failure left a provider row with no
        // config (no API key/model), then re-registered into the router as if
        // everything had been saved.
        return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    let Ok(Some(row)) = providers::get_provider(&state.db, &row.id).await else {
        return err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to reload provider after create",
        );
    };
    state.router.register_from_row(&row);

    Json(json!({"id": row.id})).into_response()
}

pub async fn update_provider(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let existing = match providers::get_provider(&state.db, &id).await {
        Ok(Some(row)) => row,
        Ok(None) => return err_response(StatusCode::NOT_FOUND, "provider not found"),
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let name = body.get("name").and_then(|v| v.as_str());
    let base_url = body.get("base_url").and_then(|v| v.as_str());
    let config = merge_config(existing.config.as_deref(), &body);

    if let Err(e) = providers::update_provider(&state.db, &id, name, base_url, Some(&config)).await
    {
        return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    if let Some(priority) = body.get("fallback_priority").and_then(|v| v.as_i64()) {
        if let Err(e) = sqlx::query(r#"UPDATE providers SET fallback_priority = ? WHERE id = ?"#)
            .bind(priority as i32)
            .bind(&id)
            .execute(&state.db)
            .await
        {
            return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
    }

    match providers::get_provider(&state.db, &id).await {
        Ok(Some(row)) => {
            state.router.register_from_row(&row);
            Json(to_public_json(&row)).into_response()
        }
        Ok(None) => err_response(StatusCode::NOT_FOUND, "provider not found"),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn delete_provider(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    state.router.unregister(&id);
    match providers::delete_provider(&state.db, &id).await {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn test_provider(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match state.router.check_health(&id).await {
        // Healthy → 200 with the latency payload.
        Ok(status) if status.status == "healthy" => Json(status).into_response(),
        // Reachable but down → 502 Bad Gateway carrying the health payload so
        // the UI can distinguish "provider exists but is down" (502) from
        // "no such provider" (404 below). Previously *every* failure was
        // reported as 404, which conflated the two.
        Ok(status) => (StatusCode::BAD_GATEWAY, Json(status)).into_response(),
        // The only `Err` `check_health` produces is an unknown provider id.
        Err(e) => err_response(StatusCode::NOT_FOUND, e.to_string()),
    }
}

pub async fn list_models(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match state.router.discover_models(&id).await {
        Ok(models) => Json(json!({"models": models})).into_response(),
        Err(e) => err_response(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_config_omitted_api_key_leaves_existing_key_untouched() {
        let existing = json!({"api_key": "sk-old", "model": "gpt-4o"}).to_string();
        let body = json!({"fallback_priority": 1});
        let merged: Value = serde_json::from_str(&merge_config(Some(&existing), &body)).unwrap();
        assert_eq!(merged["api_key"], "sk-old");
    }

    #[test]
    fn merge_config_string_api_key_is_encrypted_not_stored_as_plaintext() {
        let existing = json!({"api_key": "sk-old"}).to_string();
        let body = json!({"api_key": "sk-new"});
        let merged: Value = serde_json::from_str(&merge_config(Some(&existing), &body)).unwrap();
        let stored = merged["api_key"].as_str().unwrap();
        assert_ne!(stored, "sk-new");
        assert!(stored.starts_with("enc:v1:"));
    }

    #[test]
    fn merge_config_string_api_key_round_trips_through_decrypt() {
        let body = json!({"api_key": "sk-new"});
        let merged: Value = serde_json::from_str(&merge_config(None, &body)).unwrap();
        let stored = merged["api_key"].as_str().unwrap();
        assert_eq!(crypto::decrypt(stored), "sk-new");
    }

    #[test]
    fn merge_config_explicit_null_api_key_clears_existing_key() {
        let existing = json!({"api_key": "sk-old", "model": "gpt-4o"}).to_string();
        let body = json!({"api_key": null});
        let merged: Value = serde_json::from_str(&merge_config(Some(&existing), &body)).unwrap();
        assert!(merged.get("api_key").is_none());
        assert_eq!(merged["model"], "gpt-4o");
    }
}
