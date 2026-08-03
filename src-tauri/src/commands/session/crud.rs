//! Session create/list/resume/fork/delete — the CRUD half of session
//! lifecycle commands.

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use crate::config;
use crate::state::AppState;

use super::{chats_base_dir, resolve_cwd, ThinkingEffort, CHATS_DIR_NAME};

/// Details returned when a session is created, for the chat UI.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub cwd: String,
    pub current_mode: String,
    pub available_modes: Vec<ModeInfo>,
    /// `None` when the active model doesn't support effort control at all.
    pub thinking_effort: Option<ThinkingEffort>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModeInfo {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// Start a new session. An explicit `cwd` (e.g. a dropped folder) overrides
/// the default per-chat folder. `mode` ("chat"|"agentic") seeds BigTiny's
/// directory-sandboxing scope from creation — see `bigtiny::sessions::create`.
#[tauri::command]
pub async fn new_session(
    app: AppHandle,
    cwd: Option<String>,
    mode: Option<String>,
) -> Result<SessionInfo, String> {
    let cwd = match cwd {
        Some(c) if !c.trim().is_empty() => {
            let c_for_blocking = c.clone();
            let _ =
                tokio::task::spawn_blocking(move || std::fs::create_dir_all(&c_for_blocking)).await;
            c.replace('\\', "/")
        }
        _ => resolve_cwd(&app).await,
    };
    crate::bigtiny::sessions::create(&app, cwd, mode).await
}

/// Attach one recipe-declared extension to a live session — a no-op under
/// BigTiny, where MCP servers are daemon-global (`/api/mcp/servers`), not
/// per-session. Recipes still work; their extension hints are simply skipped
/// (best-effort, never a hard failure).
#[tauri::command]
pub async fn add_recipe_extension(
    _app: AppHandle,
    _session_id: String,
    _extension: crate::config::recipes::RecipeExtension,
) -> Result<(), String> {
    Ok(())
}

/// List past sessions (raw session objects; the frontend parses them).
#[tauri::command]
pub async fn list_sessions(app: AppHandle) -> Result<Vec<Value>, String> {
    crate::bigtiny::sessions::list(&app).await
}

/// Resume a session. The conversation replays as `chat://*` events during the
/// call; returns the session's mode info.
#[tauri::command]
pub async fn load_session(
    app: AppHandle,
    session_id: String,
    cwd: String,
) -> Result<SessionInfo, String> {
    crate::bigtiny::sessions::load(&app, session_id, cwd).await
}

/// Fork a session, optionally truncating the copy to a branch point. Powers
/// "Branch from here" and "Regenerate".
#[tauri::command]
pub async fn fork_session(
    app: AppHandle,
    session_id: String,
    cwd: String,
    truncate_from: Option<i64>,
) -> Result<SessionInfo, String> {
    crate::bigtiny::sessions::fork(&app, session_id, cwd, truncate_from).await
}

/// Record that this window is now displaying `session_id` — used so a
/// notification for that session can later be focused at the *specific*
/// window it lives in (`windows::window_label_for_session`) instead of
/// always opening a generic fallback window, and so the notification gate
/// can check whether that particular window is currently focused rather
/// than a fixed pair of singleton labels. Called by the frontend right
/// after any successful session establish/switch (`newSession`/
/// `loadSession`'s success paths) — deliberately a separate command rather
/// than folded into `new_session`/`load_session` themselves, since those are
/// also called directly with no invoking window at all (the headless
/// scheduled-task runner, `lifecycle/scheduler.rs`).
///
/// A session is shown by at most one window at a time, so before inserting
/// the new binding this clears any *other* label still pointing at the same
/// `session_id` — e.g. the overlay's own entry right after Expand hands the
/// session off to a brand-new `chat-N` window (the overlay resets its own
/// local state to blank but was never told to unbind). Without this, two
/// labels could map to the same session id at once, and
/// `window_label_for_session`'s `HashMap` scan could non-deterministically
/// return the stale one instead of the window actually showing it —
/// confirmed as the cause of "notification fires even though the right
/// window is focused" and "clicking opens the wrong window".
#[tauri::command]
pub fn bind_window_session(
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    let mut map = state.chat_windows.lock().unwrap();
    rebind_session(&mut map, window.label(), session_id);
    Ok(())
}

/// Pure logic behind `bind_window_session` — factored out so the
/// stale-binding cleanup is unit-testable without needing a real
/// `tauri::Window`/`AppHandle`.
pub(crate) fn rebind_session(
    map: &mut std::collections::HashMap<String, Option<String>>,
    keep_label: &str,
    session_id: String,
) {
    for (label, sid) in map.iter_mut() {
        if label != keep_label && sid.as_deref() == Some(session_id.as_str()) {
            *sid = None;
        }
    }
    map.insert(keep_label.to_string(), Some(session_id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn rebind_session_clears_stale_other_label() {
        // The exact bug this guards against: Expand hands session "s1" off
        // to a new "chat-1" window, but the overlay's own stale entry (still
        // pointing at "s1" since nothing ever explicitly unbinds it) must be
        // cleared, or the reverse lookup could non-deterministically pick
        // the wrong (hidden) window.
        let mut map = HashMap::new();
        map.insert("overlay".to_string(), Some("s1".to_string()));
        rebind_session(&mut map, "chat-1", "s1".to_string());
        assert_eq!(map.get("overlay"), Some(&None));
        assert_eq!(map.get("chat-1"), Some(&Some("s1".to_string())));
    }

    #[test]
    fn rebind_session_leaves_unrelated_bindings_untouched() {
        let mut map = HashMap::new();
        map.insert("main".to_string(), Some("other-session".to_string()));
        rebind_session(&mut map, "chat-1", "s1".to_string());
        assert_eq!(map.get("main"), Some(&Some("other-session".to_string())));
        assert_eq!(map.get("chat-1"), Some(&Some("s1".to_string())));
    }

    #[test]
    fn rebind_session_same_window_rebinding_same_session_stays_bound() {
        let mut map = HashMap::new();
        map.insert("chat-1".to_string(), Some("s1".to_string()));
        rebind_session(&mut map, "chat-1", "s1".to_string());
        assert_eq!(map.get("chat-1"), Some(&Some("s1".to_string())));
    }

    #[test]
    fn rebind_session_switching_a_window_to_a_different_session_updates_in_place() {
        // Same window, different session (e.g. clicking a different chat in
        // SessionList) — no other label involved, just a plain overwrite.
        let mut map = HashMap::new();
        map.insert("main".to_string(), Some("s1".to_string()));
        rebind_session(&mut map, "main", "s2".to_string());
        assert_eq!(map.get("main"), Some(&Some("s2".to_string())));
    }
}

/// Delete a session. If `cwd` sits under the private `Documents/Kitty/chats/`
/// prefix (i.e. it was never an explicit user-chosen working directory — see
/// `resolve_cwd`), also remove that directory. The prefix check is a hard
/// safety gate: a custom/explicit folder is never touched (Round-3 item 25).
#[tauri::command]
pub async fn delete_session(
    app: AppHandle,
    session_id: String,
    cwd: Option<String>,
) -> Result<(), String> {
    crate::bigtiny::sessions::delete(&app, &session_id).await?;
    // Belt-and-suspenders alongside `forActive`'s stale-event guard in
    // chatStore.ts: without this, a deleted session id could linger in
    // `in_flight_sessions` (set by `send_prompt` in prompt.rs) until its
    // in-flight turn's own cleanup runs, however long that takes.
    app.state::<AppState>()
        .in_flight_sessions
        .lock()
        .unwrap()
        .remove(&session_id);
    if let Some(cwd) = cwd {
        // Only remove a folder that sits under the chats base's `chats/` dir
        // (a Kitty-created per-chat folder) — never a user's own directory.
        let chats_root = chats_base_dir(&app).join(CHATS_DIR_NAME);
        let chats_root = format!("{}/", chats_root.to_string_lossy().replace('\\', "/"));
        let cwd_norm = cwd.replace('\\', "/");
        if cwd_norm.starts_with(&chats_root) {
            let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&cwd_norm)).await;
        }
    }
    // Cross-window live-update, mirroring `session://created` (Round-4 item 6)
    // — without this, another window's sidebar/recents keeps showing this
    // session until it happens to refresh for some other reason (confirmed
    // real gap: `regenerate()`'s background cleanup of the superseded session
    // it forked away from has no other way to reach a different window).
    let _ = app.emit("session://deleted", json!({ "sessionId": session_id }));
    Ok(())
}

/// Manual rename from the session list UI. Distinct from the auto-derived
/// title BigTiny writes after the first turn (`chat://session-title`) — this
/// is a direct user overwrite, and BigTiny never re-derives a title once one
/// is set, so a manual rename sticks.
#[tauri::command]
pub async fn rename_session(
    app: AppHandle,
    session_id: String,
    title: String,
) -> Result<(), String> {
    crate::bigtiny::sessions::rename(&app, &session_id, &title).await
}

/// Delete every session (Settings → General "Clear all chat history" — a
/// standalone destructive action, unrelated to provider switching). Also
/// clears `session_folders`/`session_modes` (app-side organization that can't
/// refer to a now-deleted session) and the active-session pointer.
#[tauri::command]
pub async fn clear_all_sessions(app: AppHandle) -> Result<usize, String> {
    let sessions = crate::bigtiny::sessions::list(&app).await?;

    let chats_root = chats_base_dir(&app).join(CHATS_DIR_NAME);
    let chats_root = format!("{}/", chats_root.to_string_lossy().replace('\\', "/"));

    let mut deleted = 0usize;
    let mut last_err: Option<String> = None;
    for s in &sessions {
        let Some(sid) = s.get("sessionId").and_then(|v| v.as_str()) else {
            continue;
        };
        match crate::bigtiny::sessions::delete(&app, sid).await {
            Ok(_) => {
                deleted += 1;
                if let Some(cwd) = s.get("cwd").and_then(|v| v.as_str()) {
                    let cwd_norm = cwd.replace('\\', "/");
                    if cwd_norm.starts_with(&chats_root) {
                        let _ =
                            tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&cwd_norm))
                                .await;
                    }
                }
            }
            Err(e) => last_err = Some(e), // keep going; one bad id shouldn't abort the rest
        }
    }

    {
        let state = app.state::<AppState>();
        let mut cfg = state.config.lock().unwrap();
        cfg.session_folders.clear();
        cfg.session_modes.clear();
        config::save(&cfg).map_err(|e| e.to_string())?;
        *state.active_session.lock().unwrap() = None;
    }
    // Deliberately not re-emitting `session://active` with a null payload here
    // — `onActiveSession`'s only consumer (`main/App.tsx`) assumes a real
    // `SessionInfo` and dereferences `info.session_id` unconditionally, so a
    // null payload would throw there. `session://cleared` below is what
    // already-open windows react to instead (SessionList/chatStore both blank
    // their own state directly, the same pattern `handOffToMain` uses).
    let _ = app.emit("session://cleared", json!({ "deleted": deleted }));

    match last_err {
        Some(e) if deleted == 0 => Err(e),
        _ => Ok(deleted),
    }
}
