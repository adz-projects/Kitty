//! Session create/list/resume/fork/delete — the CRUD half of session
//! lifecycle commands.

use std::path::{Path, PathBuf};

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
    /// True when `cwd` is a private per-chat folder (no explicit working
    /// directory chosen) — the chat header renders this as "thought partner".
    pub is_default_folder: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModeInfo {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// Start a new session. An explicit `cwd` (e.g. a dropped folder) overrides
/// the default per-chat folder. The session is stamped with the currently-active provider/model from birth
/// (per-session provider isolation), so a later global provider change never
/// retroactively flips this session.
#[tauri::command]
pub async fn new_session(app: AppHandle, cwd: Option<String>) -> Result<SessionInfo, String> {
    let cwd = match cwd {
        Some(c) if !c.trim().is_empty() => {
            let c_for_blocking = c.clone();
            tokio::task::spawn_blocking(move || std::fs::create_dir_all(&c_for_blocking))
                .await
                .map_err(|e| format!("working-directory creation task panicked: {e}"))?
                .map_err(|e| format!("could not create the working directory {c}: {e}"))?;
            c.replace('\\', "/")
        }
        _ => resolve_cwd(&app).await?,
    };
    // Resolve the global default provider/model to pin onto this session.
    let active: (Option<String>, Option<String>) = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        let provider = cfg
            .active_provider_id
            .as_deref()
            .and_then(|id| cfg.providers.iter().find(|p| p.id == id))
            .map(|p| (Some(p.id.clone()), p.models.first().cloned()));
        provider.unwrap_or((None, None))
    };
    crate::bigtiny::sessions::create(&app, cwd, active.0, active.1).await
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

/// Manually compact the current session's context (`/compact`). Forwards to
/// the daemon's `POST /api/chat/{id}/compact` and returns the result
/// `{compacted, messages_compacted, tokens_before, tokens_after}` so the UI
/// can show what was folded.
#[tauri::command]
pub async fn compact_session(
    app: AppHandle,
    session_id: String,
) -> Result<serde_json::Value, String> {
    crate::bigtiny::sessions::compact(&app, &session_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn temp_chats_root(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kitty-chats-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = dir.join("chats");
        // Anchors the canonical root (creates it if missing, like
        // `chat_folder_is_deletable` itself does).
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn chat_folder_is_deletable_allows_a_real_session_folder() {
        let root = temp_chats_root("real");
        let session = root.join("s1");
        {
            // The per-chat folder is created (as `resolve_cwd` would).
            std::fs::create_dir_all(&session).unwrap();
        }
        assert!(chat_folder_is_deletable(&session.to_string_lossy(), &root));
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn chat_folder_is_deletable_accepts_a_subdirectory_of_a_session() {
        let root = temp_chats_root("sub");
        let sub = root.join("s1").join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(chat_folder_is_deletable(&sub.to_string_lossy(), &root));
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn chat_folder_is_deletable_rejects_dot_dot_escape_only_a_string_check_misses() {
        let root = temp_chats_root("escape");
        let outside = root.parent().unwrap().join("Other");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(root.join("X")).unwrap();

        // "<base>/chats/X/../../Other" starts with "<base>/chats/" so the old
        // string-prefix check passed, but it canonicalizes to a *sibling* of
        // the chats tree — must be refused.
        let crafted = format!("{}/X/../../Other", root.to_string_lossy().replace('\\', "/"));
        assert!(!chat_folder_is_deletable(&crafted, &root));
        assert!(outside.exists(), "the outside dir must survive untouched");
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn chat_folder_is_deletable_distinguishes_a_chats2_sibling() {
        // `C:\chats` vs `C:\chats2` — the component-boundary case.
        let root = temp_chats_root("boundary");
        let sibling = root.parent().unwrap().join("chats2");
        std::fs::create_dir_all(sibling.join("foo")).unwrap();
        assert!(!chat_folder_is_deletable(&sibling.join("foo").to_string_lossy(), &root));
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn chat_folder_is_deletable_rejects_an_absolute_outside_path() {
        let root = temp_chats_root("outside");
        let elsewhere = std::env::temp_dir().join(format!(
            "kitty-chats-elsewhere-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&elsewhere).unwrap();
        assert!(!chat_folder_is_deletable(&elsewhere.to_string_lossy(), &root));
        let _ = std::fs::remove_dir_all(&elsewhere);
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn chat_folder_is_deletable_rejects_a_missing_cwd() {
        let root = temp_chats_root("missing");
        let ghost = root.join("never-created");
        assert!(!chat_folder_is_deletable(&ghost.to_string_lossy(), &root));
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn chat_folder_is_deletable_never_deletes_the_chats_root_itself() {
        // "…/chats/X/.." canonicalizes to the chats root — the boundary, not
        // strictly inside. Allowing it would nuke every chat folder at once.
        let root = temp_chats_root("root");
        std::fs::create_dir_all(root.join("X")).unwrap();
        let crafted = format!("{}/X/..", root.to_string_lossy().replace('\\', "/"));
        assert!(!chat_folder_is_deletable(&crafted, &root));
        assert!(root.exists(), "the chats root itself must never be deletable");
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

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

/// True when `cwd` names a directory *strictly inside* the chats tree
/// (`<base>/chats`). Both sides are canonicalized first, so a `cwd` like
/// `<chats>/X/../../Other` — which passes a raw string-prefix check yet
/// resolves outside the chats tree — is correctly rejected. A `cwd` that
/// can't be canonicalized (doesn't exist on disk) is never deletable.
///
/// The comparison is component-based (`strip_prefix`), so `C:\chats` can
/// never claim `C:\chats2\foo` as a descendant. Windows canonicalization
/// produces the same on-disk casing on both sides, so the component match is
/// reliable there too.
fn chat_folder_is_deletable(cwd: &str, chats_root: &Path) -> bool {
    let Ok(root) = canonicalize_or_create(chats_root) else {
        return false;
    };
    let Ok(cwd) = std::fs::canonicalize(cwd) else {
        return false;
    };
    // `strip_prefix` on canonical paths — a path that resolves to the root
    // itself (e.g. `…/chats/X/..`) has an empty remainder and is NOT strictly
    // inside: deleting it would take out every chat folder at once.
    match cwd.strip_prefix(&root) {
        Ok(rel) => !rel.as_os_str().is_empty(),
        Err(_) => false,
    }
}

/// `chats_root` may not exist yet (a fresh install with no sessions); give the
/// canonicalizer a stable anchor by creating it first.
fn canonicalize_or_create(dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    std::fs::canonicalize(dir)
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
        // Only remove a folder that sits strictly inside the chats base's
        // `chats/` dir (a Kitty-created per-chat folder) — never a user's own
        // directory. Canonicalized (not a raw string prefix), so a malicious
        // `…/chats/X/../../Other` can't redirect the delete outside the tree.
        let chats_root = chats_base_dir(&app).join(CHATS_DIR_NAME);
        if chat_folder_is_deletable(&cwd, &chats_root) {
            let cwd_for_delete = cwd.replace('\\', "/");
            let _ =
                tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&cwd_for_delete))
                    .await;
        } else {
            tracing::warn!("refusing to delete session folder outside the chats tree: {cwd}");
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
/// clears `session_folders` (app-side organization that can't refer to a
/// now-deleted session) and the active-session pointer.
#[tauri::command]
pub async fn clear_all_sessions(app: AppHandle) -> Result<usize, String> {
    let sessions = crate::bigtiny::sessions::list(&app).await?;

    let chats_root = chats_base_dir(&app).join(CHATS_DIR_NAME);

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
                    // Same canonical, strictly-inside guard as `delete_session`
                    // — never let a crafted cwd redirect the folder cleanup
                    // outside the chats tree.
                    if chat_folder_is_deletable(cwd, &chats_root) {
                        let cwd_for_delete = cwd.replace('\\', "/");
                        let _ = tokio::task::spawn_blocking(move || {
                            std::fs::remove_dir_all(&cwd_for_delete)
                        })
                        .await;
                    } else {
                        tracing::warn!(
                            "refusing to delete session folder outside the chats tree: {cwd}"
                        );
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
