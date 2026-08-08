//! `/api/mcp/servers` routes — mirrors
//! `plugins/bigtiny/bigtiny/server/routes/mcp.py`. Kitty's client
//! (`src-tauri/src/bigtiny/mcp.rs::parse_server`) expects `args`/`env`/
//! `headers` as JSON-*string* columns on each row (it parses them client
//! side), so create/update store them stringified, matching the existing
//! `mcp_servers` table shape.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use sqlx::Connection;

use crate::crypto;
use crate::storage::mcp_servers;

use super::AppState;

fn err_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

/// Encrypt every value in a `headers` JSON object before it's persisted —
/// treated as a generic header map (not just `Authorization`), since a
/// power-user editing the raw config elsewhere could put a credential in
/// any header name. Non-string values pass through unchanged (not a
/// documented shape, but not this function's job to reject).
///
/// `headers` goes through the same double-encode unwrap as `args`/`env`
/// (see `normalize_json_field`) first: a client that sends an
/// already-stringified JSON object here previously fell straight into the
/// `as_object()` `None` branch below and got stored as a JSON-string-of-a-
/// JSON-string, silently dropping the header map (including any auth
/// header) at connect time.
fn encrypt_headers(headers: &Value) -> String {
    let normalized: Value = match headers {
        Value::String(s) => serde_json::from_str::<Value>(s).unwrap_or_else(|_| headers.clone()),
        other => other.clone(),
    };
    let Some(obj) = normalized.as_object() else {
        return normalized.to_string();
    };
    let encrypted: serde_json::Map<String, Value> = obj
        .iter()
        .map(|(k, v)| {
            let v = match v.as_str() {
                Some(s) => json!(crypto::encrypt(s)),
                None => v.clone(),
            };
            (k.clone(), v)
        })
        .collect();
    Value::Object(encrypted).to_string()
}

/// Inverse of `encrypt_headers`, for the HTTP response: never echo a
/// configured header's real (encrypted or legacy-plaintext) value — mask it
/// instead. Keeps the same `{header name: value}` shape Kitty's frontend
/// already types this as, rather than collapsing to a boolean.
fn redact_headers(row: &mcp_servers::MCPServerRow) -> Value {
    let mut json = serde_json::to_value(row).unwrap_or_else(|_| json!({}));
    if let Some(headers_str) = row.headers.as_deref() {
        if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(headers_str) {
            let masked: serde_json::Map<String, Value> = obj
                .into_iter()
                .map(|(k, v)| (k, if v.is_string() { json!("***") } else { v }))
                .collect();
            json["headers"] = Value::String(Value::Object(masked).to_string());
        }
    }
    json
}

pub async fn list_servers(State(state): State<Arc<AppState>>) -> Response {
    match mcp_servers::list_servers(&state.db).await {
        Ok(rows) => {
            let redacted: Vec<Value> = rows.iter().map(redact_headers).collect();
            Json(json!({"servers": redacted})).into_response()
        }
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Normalize a JSON field like `args`/`env` before persisting it as a string
/// column. The values are later decoded at connect time via
/// `serde_json::from_str::<Vec<String>>`/`<Value>` — so a client that sends
/// the field as an ALREADY-stringified JSON string (the natural shape when
/// editing raw config, or a client that stringifies twice) must be unwrapped
/// once, or the stored value is a JSON string *containing* a JSON string,
/// which fails to parse into the expected array at connect and gets silently
/// dropped (args/env/headers gone — for `headers`, potentially the auth
/// header). Arrays/objects pass through as-is; an unparseable plain string is
/// kept verbatim (same as the old behavior).
fn normalize_json_field(v: &Value) -> String {
    match v {
        Value::Array(_) | Value::Object(_) => v.to_string(),
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Array(_)) | Ok(Value::Object(_)) => s.clone(),
            _ => v.to_string(),
        },
        _ => v.to_string(),
    }
}

pub async fn create_server(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let (Some(name), Some(transport)) = (
        body.get("name").and_then(|v| v.as_str()),
        body.get("transport").and_then(|v| v.as_str()),
    ) else {
        return err_response(StatusCode::BAD_REQUEST, "name and transport are required");
    };

    let id = uuid::Uuid::new_v4().to_string();
    let command = body.get("command").and_then(|v| v.as_str());
    let url = body.get("url").and_then(|v| v.as_str());
    let args = body.get("args").map(normalize_json_field);
    let env = body.get("env").map(normalize_json_field);
    let headers = body.get("headers").map(encrypt_headers);
    let enabled = body
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    // INSERT + connection-config UPDATE, atomically — these used to be two
    // independent statements against the pool; a failure in the second
    // (e.g. a bad `headers` value) left a half-created row (name/transport
    // only, no connection config) with no rollback.
    let mut conn = match state.db.acquire().await {
        Ok(c) => c,
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let result = async {
        let mut tx = conn.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(r#"INSERT INTO mcp_servers (id, name, transport) VALUES (?, ?, ?)"#)
            .bind(&id)
            .bind(name)
            .bind(transport)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            r#"UPDATE mcp_servers SET command = ?1, args = ?2, url = ?3, env = ?4, headers = ?5, enabled = ?6 WHERE id = ?7"#,
        )
        .bind(command)
        .bind(&args)
        .bind(url)
        .bind(&env)
        .bind(&headers)
        .bind(enabled as i32)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok::<(), sqlx::Error>(())
    }
    .await;
    if let Err(e) = result {
        return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    Json(json!({"id": id})).into_response()
}

pub async fn update_server(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let name = body.get("name").and_then(|v| v.as_str());
    let transport = body.get("transport").and_then(|v| v.as_str());
    let url = body.get("url").and_then(|v| v.as_str());
    let enabled = body
        .get("enabled")
        .and_then(|v| v.as_bool())
        .map(|b| b as i32);

    let command = body.get("command").and_then(|v| v.as_str());
    let args = body.get("args").map(normalize_json_field);
    let env = body.get("env").map(normalize_json_field);
    let headers = body.get("headers").map(encrypt_headers);
    let patch_all =
        command.is_some() || args.is_some() || env.is_some() || headers.is_some();

    // name/transport/url/enabled + command/args/env/headers, together,
    // atomically — two independent statements used to let a second-statement
    // failure (bad header value etc.) land a partial patch and then 500 with
    // the server half-updated.
    let mut conn = match state.db.acquire().await {
        Ok(c) => c,
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let result = async {
        let mut tx = conn.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            r#"UPDATE mcp_servers SET
               name = COALESCE(?1, name),
               transport = COALESCE(?2, transport),
               url = COALESCE(?3, url),
               enabled = COALESCE(?4, enabled)
               WHERE id = ?5"#,
        )
        .bind(name)
        .bind(transport)
        .bind(url)
        .bind(enabled)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        if patch_all {
            // command/args/env/headers aren't covered by the COALESCE set —
            // patch them only when actually present in the request body (an
            // absent field keeps its current value).
            sqlx::query(
                r#"UPDATE mcp_servers SET
                   command = COALESCE(?1, command),
                   args = COALESCE(?2, args),
                   env = COALESCE(?3, env),
                   headers = COALESCE(?4, headers)
                   WHERE id = ?5"#,
            )
            .bind(command)
            .bind(&args)
            .bind(&env)
            .bind(&headers)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok::<(), sqlx::Error>(())
    }
    .await;
    if let Err(e) = result {
        return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    let row = match mcp_servers::get_server(&state.db, &id).await {
        Ok(Some(row)) => row,
        Ok(None) => return err_response(StatusCode::NOT_FOUND, "server not found"),
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    // Only churn the live connection when something connection-relevant
    // actually changed — a rename-only PATCH (or any PATCH that doesn't
    // touch these fields) used to unconditionally disconnect+reconnect
    // every server on every edit, briefly killing an otherwise-healthy
    // connection and repeatedly retrying an already-broken one. Also: a
    // PATCH omitting `enabled` used to default to `true` here, silently
    // re-enabling/reconnecting an intentionally-disabled server — the
    // fetched row's actual (possibly untouched) `enabled` is used instead.
    let connection_relevant = [
        "command",
        "args",
        "env",
        "headers",
        "url",
        "transport",
        "enabled",
    ]
    .iter()
    .any(|k| body.get(*k).is_some());
    if connection_relevant {
        state.mcp.disconnect_server(&id).await;
        if row.enabled != 0 {
            let _ = state.mcp.connect_server(&id).await;
        }
    }

    Json(redact_headers(&row)).into_response()
}

pub async fn delete_server(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    state.mcp.disconnect_server(&id).await;
    match mcp_servers::delete_server(&state.db, &id).await {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn connect_server(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    match state.mcp.connect_server(&id).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_response(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}

pub async fn list_tools(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let tools = state.mcp.list_tools(Some(&id));
    Json(json!({"tools": tools})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression (88bugs #68): a `headers` value sent as an already-
    /// stringified JSON object (the exact "client stringifies twice" shape
    /// `normalize_json_field`'s doc comment describes for `args`/`env`) must
    /// be unwrapped, not stored as a JSON-string-of-a-JSON-string. Before the
    /// fix, `encrypt_headers` only handled `Value::Object`, so this shape hit
    /// `as_object() == None` and fell straight into the `to_string()`
    /// passthrough, silently dropping the header map (including any auth
    /// header) at connect time.
    #[test]
    fn encrypt_headers_unwraps_double_encoded_string() {
        let double_encoded = Value::String(r#"{"Authorization":"Bearer secret"}"#.to_string());
        let stored = encrypt_headers(&double_encoded);
        let parsed: Value = serde_json::from_str(&stored)
            .expect("stored headers must be a parseable JSON object, not a re-stringified string");
        let obj = parsed.as_object().expect("stored headers must decode to an object");
        let encrypted_auth = obj
            .get("Authorization")
            .and_then(|v| v.as_str())
            .expect("Authorization header must survive normalization");
        assert_eq!(crypto::decrypt(encrypted_auth), "Bearer secret");
    }

    /// The plain (already-an-object) shape must keep working exactly as before.
    #[test]
    fn encrypt_headers_still_handles_plain_object() {
        let plain = json!({"X-Api-Key": "abc123"});
        let stored = encrypt_headers(&plain);
        let parsed: Value = serde_json::from_str(&stored).unwrap();
        let encrypted = parsed["X-Api-Key"].as_str().unwrap();
        assert_eq!(crypto::decrypt(encrypted), "abc123");
    }
}
