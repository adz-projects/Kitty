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
fn encrypt_headers(headers: &Value) -> String {
    let Some(obj) = headers.as_object() else {
        return headers.to_string();
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
    if let Err(e) = mcp_servers::create_server(&state.db, &id, name, transport).await {
        return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    let command = body.get("command").and_then(|v| v.as_str());
    let url = body.get("url").and_then(|v| v.as_str());
    let args = body.get("args").map(|v| v.to_string());
    let env = body.get("env").map(|v| v.to_string());
    let headers = body.get("headers").map(encrypt_headers);
    let enabled = body
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    if let Err(e) = sqlx::query(
        r#"UPDATE mcp_servers SET command = ?1, args = ?2, url = ?3, env = ?4, headers = ?5, enabled = ?6 WHERE id = ?7"#,
    )
    .bind(command)
    .bind(&args)
    .bind(url)
    .bind(&env)
    .bind(&headers)
    .bind(enabled as i32)
    .bind(&id)
    .execute(&state.db)
    .await
    {
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

    if let Err(e) = mcp_servers::update_server(&state.db, &id, name, transport, url, enabled).await
    {
        return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    // command/args/env/headers aren't covered by `update_server`'s COALESCE
    // set — patch them directly, only touching fields actually present in
    // the request body (an absent field keeps its current value).
    if body.get("command").is_some()
        || body.get("args").is_some()
        || body.get("env").is_some()
        || body.get("headers").is_some()
    {
        let command = body.get("command").and_then(|v| v.as_str());
        let args = body.get("args").map(|v| v.to_string());
        let env = body.get("env").map(|v| v.to_string());
        let headers = body.get("headers").map(encrypt_headers);
        let _ = sqlx::query(
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
        .execute(&state.db)
        .await;
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
