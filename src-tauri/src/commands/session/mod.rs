//! Session lifecycle commands: create/send/cancel/approve/resume/fork/delete,
//! plus the mode-override and private-chat-folder helpers that only exist to
//! support them.

mod config;
mod crud;
mod prompt;

pub use config::*;
pub use crud::*;
pub use prompt::*;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};

use crate::state::AppState;

/// Store the active session (raw JSON) so the full window can adopt it on Expand.
/// Emits `session://active` so an *already-open* main window re-adopts the newly
/// handed-off session (its mount-time `getActiveSession` only runs once).
#[tauri::command]
pub fn set_active_session(app: AppHandle, info: Value) -> Result<(), String> {
    *app.state::<AppState>().active_session.lock().unwrap() = Some(info.clone());
    let _ = app.emit("session://active", info);
    Ok(())
}

/// Read the active session, if any (the full window calls this on mount).
#[tauri::command]
pub fn get_active_session(state: tauri::State<'_, AppState>) -> Result<Option<Value>, String> {
    Ok(state.active_session.lock().unwrap().clone())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffortOption {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingEffort {
    pub current_value: String,
    pub options: Vec<EffortOption>,
}

/// Prefix every no-explicit-folder session's private chat folder lives under
/// (Round-3 item 25). `delete_session` only ever removes a directory under
/// this prefix — never a user-chosen custom working directory.
pub const CHATS_DIR_NAME: &str = "chats";

/// Base directory that holds every chat's own context folder. The user's choice
/// (`default_context_folder`, set in Settings) when non-empty, else the default
/// `~/Documents/Kitty`. Each chat then lives in `<base>/chats/<id>/`, so the
/// setting is a *base for per-chat folders*, not one shared working directory.
///
/// **Android:** there is no user-facing Documents directory (`dirs::document_dir()`
/// returns `None`) and the process's working directory is the read-only `/`, so
/// the old `PathBuf::from(".")` fallback produced a relative `./Kitty/chats/…`
/// that failed on `create_dir_all` with `Read-only file system (os error 30)`.
/// Fall back to the app-private writable base (`config::config_dir()`,
/// `/data/user/0/<pkg>/files/Kitty`) instead — the same base every other Kitty
/// path already uses on Android. The desktop default (`~/Documents/Kitty`) is
/// unchanged; only the *unresolvable* case now lands somewhere writable rather
/// than on a relative path.
fn chats_base_dir(app: &AppHandle) -> PathBuf {
    let configured = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        cfg.default_context_folder.clone()
    };
    base_for(configured)
}

/// The chats base a `default_context_folder` setting resolves to.
pub(crate) fn base_for(configured: Option<String>) -> PathBuf {
    if let Some(dir) = configured.filter(|s| !s.trim().is_empty()) {
        return PathBuf::from(dir);
    }
    // No explicit folder chosen. Prefer the user-visible Documents dir on
    // desktop; on Android (and any host where `dirs` can't answer) use the
    // app-private writable base rather than a relative path against a
    // read-only cwd.
    if let Some(docs) = dirs::document_dir() {
        return docs.join("Kitty");
    }
    crate::config::config_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// True when `cwd` is a Kitty-managed private per-chat folder under
/// `<base>/chats/` — i.e. no explicit working directory was chosen. The chat
/// UI renders this as the "thought partner" state (no folder pill, no project
/// folder). A normalized string-prefix check rather than canonicalization: it
/// must give a stable answer for a session whose folder may not currently
/// exist on disk, and both sides derive from Kitty's own normalized output
/// (`resolve_cwd`/`chats_base_dir`), so their casing already agrees.
pub fn is_default_folder(app: &AppHandle, cwd: &str) -> bool {
    chats_roots(app)
        .iter()
        .any(|root| strictly_inside(root, cwd))
}

/// Every `<base>/chats` directory Kitty's chat folders may live under: the
/// current base's and every earlier one's.
pub(crate) fn chats_roots(app: &AppHandle) -> Vec<PathBuf> {
    let state = app.state::<AppState>();
    let cfg = state.config.lock().unwrap();
    chats_roots_for(&cfg)
}

/// [`chats_roots`] from a config rather than the running app's (the
/// uninstall cleanup has no app).
pub(crate) fn chats_roots_for(cfg: &crate::config::Config) -> Vec<PathBuf> {
    let mut roots = vec![base_for(cfg.default_context_folder.clone()).join(CHATS_DIR_NAME)];
    roots.extend(
        cfg.chats_roots_history
            .iter()
            .map(|base| PathBuf::from(base).join(CHATS_DIR_NAME)),
    );
    roots
}

/// Lexically, `cwd` is strictly inside `root` (never the root itself).
fn strictly_inside(root: &Path, cwd: &str) -> bool {
    let root = root.to_string_lossy().replace('\\', "/");
    let root = root.trim_end_matches('/');
    let cwd = cwd.replace('\\', "/");
    let cwd = cwd.trim_end_matches('/');
    cwd.strip_prefix(root)
        .map(|rel| rel.starts_with('/'))
        .unwrap_or(false)
}

/// Copy a directory tree. Used to give a branch its own copy of its chat
/// folder (decision #42).
pub(crate) fn copy_dir_all(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// A fresh chat folder under the current base, created.
pub(crate) async fn fresh_chat_folder(app: &AppHandle) -> Result<PathBuf, String> {
    let path = new_chat_folder(&chats_base_dir(app));
    let p = path.clone();
    tokio::task::spawn_blocking(move || std::fs::create_dir_all(&p))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("could not create the chat folder {}: {e}", path.display()))?;
    Ok(path)
}

/// A fresh per-chat folder `<base>/chats/<timestamp>-<short-rand>/`. The
/// `<timestamp>-<rand>` is the chat's own id — goose's session id isn't known
/// until `session/new` returns (the cwd is passed *into* it), so a client-side
/// id names the folder instead; `session/list.cwd` maps back to it later.
fn new_chat_folder(base: &Path) -> PathBuf {
    use rand::Rng;
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let suffix: String = {
        let mut rng = rand::thread_rng();
        (0..6)
            .map(|_| format!("{:x}", rng.gen_range(0u8..16)))
            .collect()
    };
    base.join(CHATS_DIR_NAME).join(format!("{ts}-{suffix}"))
}

/// The working directory a new session starts in: a fresh per-chat folder under
/// the (configurable) chats base, created if missing. Same for both modes.
/// `create_dir_all` runs on a blocking thread — this is user-triggered
/// (every "New Session"), so a slow disk shouldn't stall the tokio worker
/// other requests are running on. A mkdir failure is *propagated*, not
/// swallowed: a session created against a folder that doesn't exist is worse
/// than no session (815bugs #22).
async fn resolve_cwd(app: &AppHandle) -> Result<String, String> {
    let path = new_chat_folder(&chats_base_dir(app));
    let path_for_blocking = path.clone();
    tokio::task::spawn_blocking(move || std::fs::create_dir_all(&path_for_blocking))
        .await
        .map_err(|e| format!("chat folder creation task panicked: {e}"))?
        .map_err(|e| format!("could not create the chat folder {}: {e}", path.display()))?;
    Ok(path.to_string_lossy().replace('\\', "/"))
}
