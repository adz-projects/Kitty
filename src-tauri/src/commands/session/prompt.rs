//! Turn submission: send/cancel a prompt and respond to tool-approval prompts.

use serde::Deserialize;
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};

use crate::config::providers;
use crate::goosed::api;
use crate::notifications;
use crate::state::AppState;

/// An image attached to a chat turn (Round-3 item 17). `data_url` is a
/// `data:<mime>;base64,<...>` string as produced by `read_file_any`.
#[derive(Debug, Clone, Deserialize)]
pub struct ImageAttachment {
    pub mime: String,
    pub data_url: String,
}

/// Send a user turn (ACP `session/prompt`). Returns immediately; streamed
/// output arrives via `chat://*` events, and completion via `chat://complete`.
/// `images`, when present, are appended as native ACP image content blocks
/// (`{type:"image", data, mimeType}`, confirmed live — see acp-protocol.md)
/// instead of relying on a filesystem tool to open a path — this is what fixes
/// the "file not found" failure untrusted/remote providers hit on a bare path
/// reference (Round-3 item 17).
#[tauri::command]
pub async fn send_prompt(
    app: AppHandle,
    session_id: String,
    text: String,
    images: Option<Vec<ImageAttachment>>,
) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    // Per-provider override for how long `session/prompt` tolerates silence
    // before giving up (Settings → Providers → Advanced) — falls back to the
    // shared default. Resolved once up front since it's a plain config read.
    let idle_secs = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        cfg.active_provider_id
            .as_ref()
            .and_then(|id| cfg.providers.iter().find(|p| &p.id == id))
            .and_then(|p| p.prompt_idle_timeout_secs)
            .map(u64::from)
            .unwrap_or(api::DEFAULT_PROMPT_IDLE_SECS)
    };
    let app_bg = app.clone();
    let sid = session_id.clone();
    let mut prompt = vec![json!({ "type": "text", "text": text })];
    for img in images.unwrap_or_default() {
        // Strip a "data:<mime>;base64," prefix if present; ACP wants raw base64.
        let data = img
            .data_url
            .split_once(",")
            .map(|(_, b64)| b64)
            .unwrap_or(&img.data_url);
        prompt.push(json!({ "type": "image", "data": data, "mimeType": img.mime }));
    }
    app.state::<AppState>()
        .in_flight_sessions
        .lock()
        .unwrap()
        .insert(sid.clone());
    tauri::async_runtime::spawn(async move {
        let params = json!({ "sessionId": sid, "prompt": prompt });
        let mut res = client
            .request_session_prompt(&sid, params.clone(), idle_secs)
            .await;

        // A silent, single retry — specifically for goosed's generic
        // "Internal error" (the JSON-RPC catch-all code, confirmed via a real
        // report: a correctly-configured custom-OpenAI provider reached over
        // Tailscale "works most of the time" but fails intermittently with
        // exactly this). This is goosed *responding* (not a dead connection —
        // that surfaces as "ACP connection closed"/"ACP request cancelled",
        // different messages, not retried here), so the local ACP link is
        // fine; the failure is goosed's own upstream call to the remote
        // provider hitting a transient hiccup. Resending the identical prompt
        // once gives that upstream call a second chance before making the
        // user manually resend or restart goosed for what's often just one
        // bad round trip.
        if let Err(message) = &res {
            if message.eq_ignore_ascii_case("internal error") {
                res = client.request_session_prompt(&sid, params, idle_secs).await;
            }
        }

        match res {
            Ok(result) => {
                let _ = app_bg.emit(
                    "chat://complete",
                    json!({ "session_id": sid, "result": result }),
                );
                notifications::notify_if_hidden(
                    &app_bg,
                    notifications::Event::TaskComplete,
                    "Kitty finished",
                    "Your task is complete.",
                );
                providers::emit_health_from_send_result(&app_bg, true);
            }
            Err(message) => {
                // A failed round trip is a signal the shared ACP connection
                // may no longer be good (e.g. a plain "Invalid params" right
                // after a provider switch, or a genuine timeout) — drop
                // Kitty's client reference so the next attempt reconnects.
                // No goosed restart here: the previous idle-reset timeout had
                // a real bug (a stale activity timestamp from the *previous*
                // turn could make a fresh send time out instantly, regardless
                // of connection health — now fixed at the source in
                // `request_session_prompt`), so a genuine timeout reaching
                // here should be rare and doesn't warrant disrupting every
                // other session sharing this goosed process.
                *app_bg.state::<AppState>().acp.lock().await = None;
                let _ = app_bg.emit(
                    "chat://error",
                    json!({ "session_id": sid, "message": &message }),
                );
                notifications::notify_if_hidden(
                    &app_bg,
                    notifications::Event::TaskFailed,
                    "Kitty ran into a problem",
                    &message,
                );
                providers::emit_health_from_send_result(&app_bg, false);
            }
        }
        // A finished turn clears any pending-approval tray state.
        notifications::set_tray_pending(&app_bg, false);
        app_bg
            .state::<AppState>()
            .in_flight_sessions
            .lock()
            .unwrap()
            .remove(&sid);
    });
    Ok(())
}

/// Cancel the in-flight turn for a session (ACP `session/cancel` notification).
/// goosed resolves the pending prompt with a `cancelled` stop reason.
#[tauri::command]
pub async fn cancel_prompt(app: AppHandle, session_id: String) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    client
        .notify("session/cancel", json!({ "sessionId": session_id }))
        .await;
    Ok(())
}

/// Whether `session_id` currently has a `session/prompt` in flight — checked
/// fresh (not a client-cached snapshot) so a window adopting the session
/// (Expand mid-stream, or just resuming one another window/process is
/// actively driving) can correctly show "still working" instead of looking
/// stalled just because `session/load`'s replay doesn't reliably convey an
/// in-progress turn.
#[tauri::command]
pub fn is_session_busy(state: tauri::State<'_, AppState>, session_id: String) -> bool {
    state
        .in_flight_sessions
        .lock()
        .unwrap()
        .contains(&session_id)
}

/// Respond to a deferred tool-approval prompt. `option_id` = the chosen ACP
/// option (e.g. `allow_once`, `reject_once`); `None` cancels.
#[tauri::command]
pub async fn respond_permission(
    app: AppHandle,
    tool_call_id: String,
    option_id: Option<String>,
) -> Result<(), String> {
    let client = api::ensure_client(&app).await?;
    let id = client
        .take_permission(&tool_call_id)
        .await
        .ok_or("that approval request is no longer pending")?;

    let outcome = match option_id {
        Some(opt) => json!({ "outcome": { "outcome": "selected", "optionId": opt } }),
        None => json!({ "outcome": { "outcome": "cancelled" } }),
    };
    client.respond(id, outcome).await;
    notifications::set_tray_pending(&app, false);
    Ok(())
}
