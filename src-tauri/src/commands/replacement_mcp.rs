//! Commands for the `replacement-mcp` extension. Unlike Adaptive Pathway (a
//! Kitty-managed HTTP sidecar), replacement-mcp is a goosed-spawned stdio MCP
//! extension — Kitty's only job is keeping its registration entry in goose's
//! own config.yaml (see `goose_config.rs`) pointed at the current install's
//! bundled exe, and toggling it on/off. No `ManagedProcess`, no health probe:
//! goosed owns the process, and its liveness already flows through goosed's
//! own `extensions/list`.

use crate::config;
use crate::goose_config;
use crate::state::AppState;

const REPLACEMENT_MCP_EXTENSION_ID: &str = "replacement-mcp";
const DEVELOPER_EXTENSION_ID: &str = "developer";
const COMPUTER_CONTROLLER_EXTENSION_ID: &str = "computercontroller";

/// Resolves the bundled `replacement-mcp.exe` next to the running app (same
/// `externalBin` convention as Adaptive Pathway's sidecar — see
/// `config::bundled_plugin_path`), falling back to a bare PATH-relative name
/// for dev convenience (pointing a local `uv run replacement-mcp` at it).
fn replacement_mcp_command() -> String {
    config::bundled_plugin_path("replacement-mcp.exe")
        .unwrap_or_else(|| "replacement-mcp".to_string())
}

/// Idempotently (re)register the extension in goose's config.yaml so `cmd`
/// always points at the current install's bundled exe — self-heals across an
/// app update/reinstall the same way Adaptive Pathway's env-var migration
/// does in `lifecycle::start_stack`. Called at every app startup; never
/// touches `enabled`, which is the user's own Settings choice.
pub fn ensure_registered() -> Result<(), String> {
    goose_config::ensure_extension_registered(
        REPLACEMENT_MCP_EXTENSION_ID,
        &replacement_mcp_command(),
        &[],
    )
}

#[tauri::command]
pub fn get_replacement_mcp_enabled(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.config.lock().unwrap().replacement_mcp_enabled)
}

/// Enable/disable the extension. Registers it first if this is the very
/// first time it's ever been toggled (a fresh install's config.yaml has no
/// entry for it yet). Does **not** touch Goose's built-in `developer`/
/// `computercontroller` extensions on its own — see
/// `disable_builtin_dev_extensions`, which the frontend calls only after the
/// user explicitly accepts that offer (CLAUDE.md B4: "surface the choice,
/// don't force it").
#[tauri::command]
pub fn set_replacement_mcp_enabled(
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    ensure_registered()?;
    goose_config::set_extension_default_enabled(REPLACEMENT_MCP_EXTENSION_ID, enabled)?;
    let mut cfg = state.config.lock().unwrap();
    cfg.replacement_mcp_enabled = enabled;
    config::save(&cfg).map_err(|e| e.to_string())
}

/// Disables Goose's built-in `developer` + `computercontroller` extensions —
/// only ever called from the frontend's explicit "replace the built-ins?"
/// offer shown when the user turns replacement-mcp on, never automatically.
#[tauri::command]
pub fn disable_builtin_dev_extensions() -> Result<(), String> {
    goose_config::set_extension_default_enabled(DEVELOPER_EXTENSION_ID, false)?;
    goose_config::set_extension_default_enabled(COMPUTER_CONTROLLER_EXTENSION_ID, false)
}
