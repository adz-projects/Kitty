//! `/api/admin` -- operations on the daemon itself rather than on one app's data.
//!
//! The daemon is shared, so no single app may simply kill it: another may be
//! mid-turn. But some settings only reach the daemon when it starts (they are
//! spawn-time environment), so an app that changes one needs a way to get a
//! fresh daemon. This is that way, with the other apps' work protected.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::storage::apps::{self, AppIdentity};
use crate::storage::sessions;

use super::AppState;

/// How recently another app must have authenticated to count as attached.
/// `last_seen_at` is written about once a minute per key, so three intervals
/// comfortably covers an app that is open but momentarily quiet.
pub const ATTACHED_WITHIN_SECS: u64 = 180;

/// Grace between answering and shutting down, so the response reaches the
/// caller before its connection is drained.
const SHUTDOWN_DELAY: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Debug, Default, Deserialize)]
pub struct RestartRequest {
    /// Restart even though other apps are attached. Never overrides a turn in
    /// progress -- the caller's or anyone else's.
    #[serde(default)]
    pub force: bool,
}

/// Why a restart is being held back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Blocker {
    pub app_id: String,
    pub display_name: String,
    /// `active_turn` (a turn, job or scheduled run is in progress) or
    /// `attached` (the app authenticated recently and may be about to).
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct RestartResponse {
    pub accepted: bool,
    pub blocked_by: Vec<Blocker>,
}

/// `POST /api/admin/restart` -- shut the daemon down so the caller can start
/// a fresh one, but only when that is safe for everyone else.
///
/// Safe means: no turn is in progress (any app's, the caller's included --
/// it should finish or cancel its own first), and no *other* app has
/// authenticated within [`ATTACHED_WITHIN_SECS`]. `force` waives only the
/// second: an app that is merely open re-attaches to the next daemon on its
/// own, but a turn cut off mid-answer is lost, so nothing waives the first.
///
/// A 200 either way; `accepted` says whether the daemon is going down.
pub async fn restart(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AppIdentity>,
    body: Option<Json<RestartRequest>>,
) -> Response {
    let force = body.map(|Json(b)| b.force).unwrap_or(false);
    let blockers = match blockers(&state, &identity.app_id, force).await {
        Ok(b) => b,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response()
        }
    };
    let accepted = blockers.is_empty();
    if accepted {
        tracing::warn!("app {:?} requested a daemon restart; shutting down", identity.app_id);
        let shutdown = state.shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SHUTDOWN_DELAY).await;
            // `notify_one` stores a permit if `run()` is not yet waiting, so
            // the request can never be lost to a race with the select.
            shutdown.notify_one();
        });
    }
    Json(RestartResponse {
        accepted,
        blocked_by: blockers,
    })
    .into_response()
}

async fn blockers(state: &AppState, caller: &str, force: bool) -> Result<Vec<Blocker>, String> {
    let names: std::collections::HashMap<String, String> = apps::list_apps(&state.db)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|a| (a.id, a.display_name))
        .collect();
    let name_of = |id: &str| names.get(id).cloned().unwrap_or_else(|| id.to_string());

    let mut out: Vec<Blocker> = Vec::new();
    let mut push = |app_id: String, reason: &str| {
        let b = Blocker {
            display_name: name_of(&app_id),
            app_id,
            reason: reason.to_string(),
        };
        if !out.contains(&b) {
            out.push(b);
        }
    };

    // Work in progress, the caller's included.
    for session_id in state.agent.active_session_ids() {
        let owner = sessions::owner_of(&state.db, &session_id)
            .await
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        push(owner, "active_turn");
    }

    // Other apps that are open right now.
    if !force {
        for id in names.keys() {
            if id == caller {
                continue;
            }
            if apps::seen_within_secs(&state.db, id, ATTACHED_WITHIN_SECS)
                .await
                .map_err(|e| e.to_string())?
            {
                push(id.clone(), "attached");
            }
        }
    }
    Ok(out)
}
