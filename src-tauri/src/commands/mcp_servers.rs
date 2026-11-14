//! MCP server management commands — the BigTiny-backed replacement for the
//! old goosed-path `commands::extensions` (which read/wrote goose's own
//! `config.yaml`) and `commands::replacement_mcp`. BigTiny's MCP servers are
//! daemon-global and managed live over REST (`bigtiny::mcp`), so every
//! command here just proxies to the running daemon — no config.yaml, no
//! restart required for add/edit/delete/toggle.

use crate::bigtiny::client::ensure_client;
use crate::bigtiny::mcp::{self, McpServer, McpServerPatch, McpServerSpec};
use crate::config;
use crate::state::AppState;

#[tauri::command]
pub async fn list_mcp_servers(app: tauri::AppHandle) -> Result<Vec<McpServer>, String> {
    let client = ensure_client(&app)?;
    mcp::list_servers(&client).await
}

#[tauri::command]
pub async fn add_mcp_server(app: tauri::AppHandle, spec: McpServerSpec) -> Result<String, String> {
    let client = ensure_client(&app)?;
    let id = mcp::create_server(&client, &spec).await?;
    if spec.enabled {
        // If the post-create auto-connect fails, roll the row back so we
        // don't leave a half-configured, never-connected server card behind
        // (mirrors the missing-cleanup gap in the old code).
        if let Err(e) = mcp::connect_server(&client, &id).await {
            let _ = mcp::delete_server(&client, &id).await;
            return Err(e);
        }
    }
    Ok(id)
}

#[tauri::command]
pub async fn update_mcp_server(
    app: tauri::AppHandle,
    id: String,
    patch: McpServerPatch,
) -> Result<McpServer, String> {
    let client = ensure_client(&app)?;
    mcp::update_server(&client, &id, &patch).await
}

#[tauri::command]
pub async fn delete_mcp_server(app: tauri::AppHandle, id: String) -> Result<(), String> {
    let client = ensure_client(&app)?;
    mcp::delete_server(&client, &id).await
}

#[tauri::command]
pub async fn set_mcp_server_enabled(
    app: tauri::AppHandle,
    id: String,
    enabled: bool,
) -> Result<McpServer, String> {
    let client = ensure_client(&app)?;
    mcp::update_server(
        &client,
        &id,
        &McpServerPatch {
            enabled: Some(enabled),
            ..Default::default()
        },
    )
    .await
}

#[tauri::command]
pub async fn connect_mcp_server(app: tauri::AppHandle, id: String) -> Result<(), String> {
    let client = ensure_client(&app)?;
    mcp::connect_server(&client, &id).await
}

/// Whether the bundled `kitty-wasm` server (sandboxed WebAssembly
/// Python/arbitrary-module execution — the Rust replacement for the retired
/// `wasm-math-mcp` Python plugin) is registered+enabled in BigTiny. No
/// credentials, so a plain toggle like `replacement_mcp` above.
#[tauri::command]
pub fn get_kitty_wasm_enabled(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.config.lock().unwrap().kitty_wasm_enabled)
}

#[tauri::command]
pub async fn set_kitty_wasm_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.kitty_wasm_enabled = enabled;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    apply(&app, "kitty-wasm", "the Python sandbox").await
}

/// Whether the bundled `visualizations` server is registered+enabled in
/// BigTiny. No credentials, so a plain toggle like `wasm_math_mcp` above.
#[tauri::command]
pub fn get_visualizations_enabled(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.config.lock().unwrap().visualizations_enabled)
}

#[tauri::command]
pub async fn set_visualizations_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.visualizations_enabled = enabled;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    apply(&app, "kitty-tools", "visualizations").await
}

/// Whether the bundled `kitty-tools` server — shell/workspace/file/word/
/// cache/scratchpad, plus Brave search and the 2 visualization tools gated
/// by their own flags — is registered+enabled in BigTiny. This flag alone
/// controls whether the whole process runs; `visualizations_enabled`/
/// `brave_mcp_search_enabled` separately control which tools it advertises
/// once running (see `bigtiny::mcp::ensure_builtin_servers`).
#[tauri::command]
pub fn get_kitty_tools_enabled(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.config.lock().unwrap().kitty_tools_enabled)
}

#[tauri::command]
pub async fn set_kitty_tools_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.kitty_tools_enabled = enabled;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    apply(&app, "kitty-tools", "the file and shell tools").await
}

/// Global master switch for delegation (specialists). When off, the
/// `specialists` in-process MCP server registers disabled, so the model is
/// never offered `call_specialist`/`await_specialists`/`list_specialists`.
/// Re-synced live via `ensure_builtin_servers` — no daemon restart.
#[tauri::command]
pub fn get_specialists_enabled(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.config.lock().unwrap().specialists.enabled)
}

#[tauri::command]
pub async fn set_specialists_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.specialists.enabled = enabled;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    apply(&app, "specialists", "specialists").await
}

/// Whether the bundled `kitty-web` web-search/web-scrape server is
/// registered+enabled in BigTiny. No credentials, so a plain toggle like
/// `wasm_math_mcp` above.
#[tauri::command]
pub fn get_kitty_web_enabled(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.config.lock().unwrap().kitty_web_enabled)
}

#[tauri::command]
pub async fn set_kitty_web_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.kitty_web_enabled = enabled;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    apply(&app, "kitty-web", "web search").await
}

/// Brave Search MCP status for Settings — `enabled` mirrors the user's
/// toggle intent, `configured` reports whether an API key is currently
/// stored in the keyring. The UI shows the API-key form whenever `!configured`,
/// regardless of `enabled`, since the two can never usefully disagree for
/// long (see `set_brave_mcp_search_enabled`).
#[derive(serde::Serialize)]
pub struct BraveMcpSearchStatus {
    pub enabled: bool,
    pub configured: bool,
}

#[tauri::command]
pub async fn get_brave_mcp_search_status(
    state: tauri::State<'_, AppState>,
) -> Result<BraveMcpSearchStatus, String> {
    let enabled = state.config.lock().unwrap().brave_mcp_search_enabled;
    // Async read: `has_secret` is blocking Windows Credential Manager IPC,
    // which must not run on a tokio worker (see `get_secret_async`).
    let configured = config::providers::get_secret_async("brave-mcp-search")
        .await
        .is_some();
    Ok(BraveMcpSearchStatus {
        enabled,
        configured,
    })
}

/// Store the Brave Search API key and enable the server in one step — the
/// only way to turn brave-mcp-search on. There is deliberately no
/// "just flip enabled=true" path: without a key the server can't do
/// anything, so enabling always means (re)configuring it fresh.
#[tauri::command]
pub async fn set_brave_mcp_search_api_key(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    api_key: String,
) -> Result<(), String> {
    let trimmed = api_key.trim();
    if trimmed.is_empty() {
        return Err("API key cannot be empty".to_string());
    }
    // Confirm the key actually works before storing it — otherwise a wrong
    // or mistyped key still renders a green "configured" checkbox and every
    // search silently comes back AUTH_ERROR with no indication why.
    mcp::validate_brave_api_key(trimmed).await?;
    config::providers::set_secret_async("brave-mcp-search", trimmed).await?;
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.brave_mcp_search_enabled = true;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    apply(&app, "kitty-web", "Brave search").await
}

/// Disabling always deletes the stored key (see `brave_mcp_search_enabled`'s
/// doc comment in `config/mod.rs`) — re-enabling therefore always goes
/// through `set_brave_mcp_search_api_key` and requires the user to type the
/// key again. There is no "disable but keep the key" — that's the entire
/// point of this flag's design.
#[tauri::command]
pub async fn set_brave_mcp_search_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    if enabled {
        return Err(
            "brave-mcp-search must be enabled via set_brave_mcp_search_api_key".to_string(),
        );
    }
    config::providers::delete_secret("brave-mcp-search");
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.brave_mcp_search_enabled = false;
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    apply(&app, "kitty-web", "Brave search").await
}

/// Push a saved toggle to the engine and report whether `server` took it
/// (#68): the toggle used to say "done" whatever happened.
pub(crate) async fn apply(app: &tauri::AppHandle, server: &str, label: &str) -> Result<(), String> {
    mcp::ensure_builtin_servers(app)
        .await
        .map_err(|e| {
            tracing::warn!("builtin MCP sync failed: {e}");
            format!("Saved, but {label} will only change once the engine is running again.")
        })?
        .check(server, label)
}

// ---------------------------------------------------------------------------
// Bacon notebook MCP server (Bacon phase 10). A PRIVATE row named "bacon"
// pointing at the bacon-mcp stdio executable — not a builtin, so it is not
// part of ensure_builtin_servers. Switched on here; off by default, and only
// offered when Bacon is installed on this machine. A private
// row (rather than a shared one) is deliberate: a shared row is immutable even
// to its creator, so a path that changes on update would be stuck.
// ---------------------------------------------------------------------------

const BACON_MCP_NAME: &str = "bacon";

#[derive(serde::Serialize)]
pub struct BaconMcpStatus {
    /// Bacon is installed here. Settings shows the switch only when this is
    /// true, or when the switch is already on (so it can still be turned off
    /// after Bacon is uninstalled).
    pub installed: bool,
    pub enabled: bool,
    pub path: Option<String>,
    pub registered: bool,
}

#[tauri::command]
pub async fn get_bacon_mcp_status(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<BaconMcpStatus, String> {
    let (enabled, path) = {
        let cfg = state.config.lock().unwrap();
        (cfg.bacon_mcp_enabled, cfg.bacon_mcp_path.clone())
    };
    let registered = match ensure_client(&app) {
        Ok(client) => mcp::list_servers(&client)
            .await
            .map(|servers| servers.iter().any(|s| s.name == BACON_MCP_NAME))
            .unwrap_or(false),
        Err(_) => false,
    };
    let installed = tauri::async_runtime::spawn_blocking(|| bacon_install_dir().is_some())
        .await
        .unwrap_or(false);
    Ok(BaconMcpStatus {
        installed,
        enabled,
        path,
        registered,
    })
}

#[tauri::command]
pub async fn set_bacon_mcp_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
    path: Option<String>,
) -> Result<(), String> {
    // Find the executable before recording anything: a switch saved as on
    // with nothing to point at would show as enabled but never register.
    let exe = if enabled {
        Some(resolve_bacon_mcp_path(path.as_deref()).ok_or(
            "Could not find the bacon-mcp executable. Build it in the Bacon repo (cargo build --release in crates/bacon-mcp) or set its path.",
        )?)
    } else {
        None
    };
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.bacon_mcp_enabled = enabled;
        cfg.bacon_mcp_path = path.clone();
        config::save(&cfg).map_err(|e| e.to_string())?;
    }
    let client = ensure_client(&app)?;
    let existing = mcp::list_servers(&client)
        .await?
        .into_iter()
        .find(|s| s.name == BACON_MCP_NAME);

    let Some(exe) = exe else {
        if let Some(row) = existing {
            mcp::delete_server(&client, &row.id).await?;
        }
        return Ok(());
    };
    let exe_str = exe.to_string_lossy().into_owned();
    match existing {
        Some(row) => {
            let patch = McpServerPatch {
                command: Some(exe_str),
                enabled: Some(true),
                ..Default::default()
            };
            mcp::update_server(&client, &row.id, &patch).await?;
            let _ = mcp::connect_server(&client, &row.id).await;
        }
        None => {
            let spec = McpServerSpec {
                name: BACON_MCP_NAME.to_string(),
                transport: "stdio".to_string(),
                command: Some(exe_str),
                args: Vec::new(),
                url: None,
                env: Default::default(),
                headers: Default::default(),
                timeout_s: Some(60),
                enabled: true,
            };
            let id = mcp::create_server(&client, &spec).await?;
            let _ = mcp::connect_server(&client, &id).await;
        }
    }
    Ok(())
}

/// Where Bacon is installed, if it is. Bacon's installer is per-user NSIS and
/// records `InstallLocation` under its uninstall key; HKCU is checked, then
/// HKLM (a per-machine install), then the per-user default folder. Each
/// candidate counts only if `bacon.exe` is actually there, so a stale key
/// left by a failed uninstall does not.
#[cfg(windows)]
fn bacon_install_dir() -> Option<std::path::PathBuf> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ};
    use winreg::RegKey;
    const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Bacon";
    let has_exe = |dir: &std::path::Path| dir.join("bacon.exe").is_file();
    for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        let location = RegKey::predef(hive)
            .open_subkey_with_flags(UNINSTALL_KEY, KEY_READ)
            .and_then(|key| key.get_value::<String, _>("InstallLocation"));
        if let Ok(location) = location {
            let dir = std::path::PathBuf::from(location.trim().trim_matches('"'));
            if has_exe(&dir) {
                return Some(dir);
            }
        }
    }
    let dir = dirs::data_local_dir()?.join("Bacon");
    has_exe(&dir).then_some(dir)
}

/// Bacon is a Windows app; there is nothing to find elsewhere.
#[cfg(not(windows))]
fn bacon_install_dir() -> Option<std::path::PathBuf> {
    None
}

/// Resolve the bacon-mcp executable: an explicit path wins; then one shipped
/// inside Bacon's install folder; then the sibling Bacon checkout's build
/// output (release, then debug). That last fallback is relative to where this
/// crate was *built*, so it only finds anything on a development machine.
fn resolve_bacon_mcp_path(explicit: Option<&str>) -> Option<std::path::PathBuf> {
    if let Some(p) = explicit.map(str::trim).filter(|p| !p.is_empty()) {
        let path = std::path::PathBuf::from(p);
        return path.is_file().then_some(path);
    }
    if let Some(exe) = bacon_install_dir()
        .map(|dir| dir.join("bacon-mcp.exe"))
        .filter(|exe| exe.is_file())
    {
        return Some(exe);
    }
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // Kitty/src-tauri -> Kitty -> sibling Bacon.
    let bacon = manifest.parent()?.parent()?.join("Bacon");
    for profile in ["release", "debug"] {
        let exe = bacon
            .join("crates")
            .join("bacon-mcp")
            .join("target")
            .join(profile)
            .join("bacon-mcp.exe");
        if exe.is_file() {
            return Some(exe);
        }
    }
    None
}
