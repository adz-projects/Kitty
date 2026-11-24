//! `/tidy` commands. A tidy is started by sending a `/tidy` message
//! (`send_prompt`'s `tidy`); these serve its card.

use serde_json::Value;
use tauri::AppHandle;

use crate::bigtiny::client::ensure_client;
use crate::bigtiny::tidy;

/// The tidy chat's candidates and which of them the model may read.
#[tauri::command]
pub async fn get_tidy(app: AppHandle, session_id: String) -> Result<Value, String> {
    let client = ensure_client(&app)?;
    tidy::state(&client, &session_id).await.map_err(|e| {
        tracing::warn!("get_tidy {session_id}: {e}");
        "Could not read the tidy list.".to_string()
    })
}

/// The user lets the model read these chats' first and last exchange; the
/// pending proposal goes back to the model to do so.
#[tauri::command]
pub async fn tidy_allow_reading(
    app: AppHandle,
    session_id: String,
    action_id: String,
    refs: Vec<String>,
) -> Result<(), String> {
    if refs.is_empty() {
        return Err("Choose the chats it may read.".into());
    }
    tidy::allow_reading(&app, &session_id, &action_id, &refs)
        .await
        .map_err(|e| {
            tracing::warn!("tidy_allow_reading {session_id}: {e}");
            "Could not pass that on.".to_string()
        })
}

/// Delete the chosen chats permanently, then the tidy chat.
#[tauri::command]
pub async fn tidy_apply(
    app: AppHandle,
    session_id: String,
    action_id: String,
    refs: Vec<String>,
) -> Result<tidy::Applied, String> {
    tidy::apply(&app, &session_id, &action_id, &refs)
        .await
        .map_err(|e| {
            tracing::warn!("tidy_apply {session_id}: {e}");
            "Could not tidy the chats.".to_string()
        })
}
