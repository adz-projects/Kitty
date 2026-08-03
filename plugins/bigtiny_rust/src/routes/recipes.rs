//! `/api/recipes` routes — mirrors
//! `plugins/bigtiny/bigtiny/server/routes/recipes.py`.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::storage::recipes;

use super::AppState;

fn err_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

pub async fn list_recipes(State(state): State<Arc<AppState>>) -> Response {
    match recipes::list_recipes(&state.db).await {
        Ok(rows) => Json(json!({"recipes": rows})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn create_recipe(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let (Some(name), Some(prompt_template)) = (
        body.get("name").and_then(|v| v.as_str()),
        body.get("prompt_template").and_then(|v| v.as_str()),
    ) else {
        return err_response(
            StatusCode::BAD_REQUEST,
            "name and prompt_template are required",
        );
    };
    let instructions = body.get("instructions").and_then(|v| v.as_str());
    let max_steps = body.get("max_steps").and_then(|v| v.as_i64()).unwrap_or(30) as i32;

    let id = uuid::Uuid::new_v4().to_string();
    match recipes::create_recipe(
        &state.db,
        &id,
        name,
        prompt_template,
        instructions,
        max_steps,
    )
    .await
    {
        Ok(()) => Json(json!({"id": id})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn delete_recipe(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match recipes::delete_recipe(&state.db, &id).await {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn execute_recipe(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    let parameters = body.map(|Json(b)| b).unwrap_or_else(|| json!({}));
    match state.recipe_engine.execute(&id, parameters).await {
        Ok(session_id) => Json(json!({"session_id": session_id})).into_response(),
        Err(e) => err_response(StatusCode::BAD_REQUEST, e.to_string()),
    }
}
