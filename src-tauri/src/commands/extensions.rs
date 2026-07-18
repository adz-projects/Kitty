//! Extension default-management commands (Round-7 Feature 4) — these read and
//! write goose's own `config.yaml` directly (see `goose_config.rs`), not any
//! single session's live state. A change here is the actual "default
//! extensions for new chats" surface, taking effect on the *next* new
//! session (a session already open is unaffected, same as a provider/
//! temperature change needing a goosed restart to apply).

use crate::goose_config::{self, ExtensionDefault};

/// The full extensions catalog — every extension goose knows about, on or
/// off, unlike the old session-scoped ACP `extensions/list` (which only ever
/// showed what was already attached to one session, giving no visibility
/// into installed-but-inactive extensions at all).
#[tauri::command]
pub fn list_default_extensions() -> Result<Vec<ExtensionDefault>, String> {
    goose_config::list_extension_defaults()
}

/// Flip one extension's default enabled state.
#[tauri::command]
pub fn set_default_extension_enabled(id: String, enabled: bool) -> Result<(), String> {
    goose_config::set_extension_default_enabled(&id, enabled)
}

/// Add a brand-new custom stdio/MCP extension as a persistent default.
#[tauri::command]
pub fn add_extension(
    name: String,
    command: String,
    args: Vec<String>,
    env: Vec<String>,
) -> Result<(), String> {
    goose_config::add_custom_extension_default(&name, &command, &args, &env)
}

/// Set one literal env value on an already-registered extension's `envs:`
/// map — for non-secret values only (see `goose_config::set_extension_env`).
/// A no-op if the extension isn't registered yet.
#[tauri::command]
pub fn set_extension_env(id: String, key: String, value: String) -> Result<(), String> {
    goose_config::set_extension_env(&id, &key, &value)
}
