//! `/api/recipes` routes — mirrors
//! `plugins/bigtiny/bigtiny/server/routes/recipes.py`.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::{json, Value};

use crate::error::RecipeError;
use crate::storage::apps::AppIdentity;
use crate::storage::recipes;

use super::AppState;

fn err_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

pub async fn list_recipes(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
) -> Response {
    match recipes::list_recipes_for_app(&state.db, &identity.app_id).await {
        Ok(rows) => Json(json!({"recipes": rows})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn create_recipe(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
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
        &identity.app_id,
    )
    .await
    {
        Ok(()) => Json(json!({"id": id})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn delete_recipe(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(id): Path<String>,
) -> Response {
    match recipes::delete_recipe_for_app(&state.db, &id, &identity.app_id).await {
        // Not ours, or gone: a 404. Reporting success for a delete that
        // matched nothing makes a client drop the row from its list and then
        // watch it reappear on the next refresh.
        Ok(0) => err_response(StatusCode::NOT_FOUND, format!("no such recipe: {id}")),
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn execute_recipe(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    // Executing another app's recipe would run its workflow on its
    // provider and against its billing account.
    match recipes::get_recipe_for_app(&state.db, &id, &identity.app_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return err_response(StatusCode::NOT_FOUND, "recipe not found"),
        Err(e) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
    let parameters = body.map(|Json(b)| b).unwrap_or_else(|| json!({}));
    match state.recipe_engine.execute(&id, parameters).await {
        Ok(session_id) => Json(json!({"session_id": session_id})).into_response(),
        Err(e) => {
            // Distinguish validation/usage errors (missing recipe → 404,
            // bad template → 400) from internal failures (storage → 500,
            // provider-failed turn → 500). The previous code mapped *every*
            // engine error to 400, hiding genuine server faults as client
            // errors.
            let status = match &e {
                RecipeError::NotFound(_) => StatusCode::NOT_FOUND,
                RecipeError::Template(_) => StatusCode::BAD_REQUEST,
                RecipeError::Storage(_) | RecipeError::TurnFailed(_) => {
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            };
            err_response(status, e.to_string())
        }
    }
}
