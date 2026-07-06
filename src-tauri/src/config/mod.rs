//! App configuration: a single JSON document at
//! `%APPDATA%/goose-overlay/config.json`, loaded at startup and written back on
//! change. Stores **metadata only** — never secrets (those live in the Windows
//! Credential Manager via `keyring`, wired in Phase 5).

pub mod env_helper;
pub mod providers;

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use providers::ProviderProfile;

/// The persisted application configuration.
///
/// `#[serde(default)]` on every field means old config files keep loading as new
/// fields are added across phases.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Accelerators that toggle the overlay (Round-2 item 3 — was a single
    /// `hotkey` string; migrated in `load`). Any of them fires the toggle.
    /// Defaults to empty (not the struct default) so `migrate_hotkeys` can tell a
    /// pre-Round-2 config apart and seed from the legacy `hotkey` field.
    #[serde(default = "Vec::new")]
    pub hotkeys: Vec<String>,
    /// Accelerator that summons the overlay with the current clipboard
    /// pre-attached (Round-4 clipboard hotkey). `None` = not registered.
    #[serde(default)]
    pub clipboard_hotkey: Option<String>,
    /// Whether to prefer the hardware Copilot key when observed (Phase 6).
    pub use_copilot_key: bool,
    /// Default working directory for new sessions (Phase 4). `None` until set.
    pub default_context_folder: Option<String>,
    /// Ollama endpoint; the stack manager probes and (if needed) spawns it.
    pub ollama_base_url: String,
    /// First-run wizard completion flag (gates the wizard in Phase 7).
    pub setup_completed: bool,
    /// Active theme name (built-ins `default`/`dark`, or a user `.css` filename).
    pub theme: String,
    /// Optional background-image path applied to all windows (Phase 6).
    pub background_image: Option<String>,
    /// Background-image dim (0.0 = none, 1.0 = fully dark overlay).
    pub background_dim: f32,
    /// Background-image position, 0.0-100.0 each axis (Round-4 item 2).
    #[serde(default = "default_bg_position")]
    pub background_position_x: f32,
    #[serde(default = "default_bg_position")]
    pub background_position_y: f32,
    /// `"cover" | "contain" | "stretch" | "center"` (Round-4 item 2) — named to
    /// match Windows' own wallpaper-fit terminology in the UI (Fill/Fit/
    /// Stretch/Center).
    #[serde(default = "default_bg_size")]
    pub background_size: String,
    /// Per-event notification preferences (surfaced in Settings in Phase 5).
    pub notifications: NotificationPrefs,
    /// Remember overlay size/position between summons (Phase 6).
    pub remember_overlay_position: bool,
    /// Provider profiles (metadata only; secrets live in the keyring).
    pub providers: Vec<ProviderProfile>,
    /// Id of the active provider profile, if any (else goosed uses its config).
    pub active_provider_id: Option<String>,
    /// Disable file/folder drop while a remote-tier provider is active.
    pub strict_remote_mode: bool,
    /// What goosed should do when a conversation approaches its context
    /// limit (Round-4 item 3): `"summarize" | "truncate" | "clear" | "prompt"`,
    /// threaded into its spawn env as `GOOSE_CONTEXT_STRATEGY`. Replaces the
    /// old `auto_summarize_threshold` message-count field, which was never
    /// actually read anywhere — Goose's real auto-compaction triggers on
    /// context-percentage, not a user-settable message count, so that field's
    /// semantics never mapped to anything real.
    #[serde(default = "default_context_strategy")]
    pub context_strategy: String,
    /// User-defined chat folders (Round-2 item 15). App-side only — layered over
    /// goosed's session list; not visible to other Goose clients.
    #[serde(default)]
    pub folders: Vec<String>,
    /// Maps a goosed session id → folder name (app-side organization only).
    #[serde(default)]
    pub session_folders: HashMap<String, String>,
    /// Whether the main window's artifacts pane is shown (Round-3 item 6).
    pub show_artifacts: bool,
    /// Per-session chat/agentic mode override (Round-4 instant mode toggle).
    /// Maps a goosed session id → `"chat"` | `"agentic"`. Absent = follow the
    /// active provider's `tools_enabled` default. Persisted (not transient) so
    /// resuming a flipped session doesn't silently revert its attachment/tool
    /// semantics — mirrors `session_folders` above.
    #[serde(default)]
    pub session_modes: HashMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkeys: vec!["Alt+Space".to_string()],
            clipboard_hotkey: Some("Ctrl+Alt+Space".to_string()),
            use_copilot_key: false,
            default_context_folder: None,
            ollama_base_url: "http://localhost:11434".to_string(),
            setup_completed: false,
            theme: "default".to_string(),
            background_image: None,
            background_dim: 0.3,
            background_position_x: default_bg_position(),
            background_position_y: default_bg_position(),
            background_size: default_bg_size(),
            notifications: NotificationPrefs::default(),
            remember_overlay_position: true,
            providers: Vec::new(),
            active_provider_id: None,
            strict_remote_mode: false,
            context_strategy: default_context_strategy(),
            folders: Vec::new(),
            session_folders: HashMap::new(),
            show_artifacts: true,
            session_modes: HashMap::new(),
        }
    }
}

fn default_bg_position() -> f32 {
    50.0
}

fn default_bg_size() -> String {
    "cover".to_string()
}

fn default_context_strategy() -> String {
    "summarize".to_string()
}

/// Per-event notification toggles.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationPrefs {
    pub task_complete: bool,
    pub approval_needed: bool,
    pub task_failed: bool,
    pub stack_degraded: bool,
}

impl Default for NotificationPrefs {
    fn default() -> Self {
        Self {
            task_complete: true,
            approval_needed: true,
            task_failed: true,
            stack_degraded: true,
        }
    }
}

/// Errors from reading/writing the config file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not resolve the app config directory")]
    NoConfigDir,
    #[error("config i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("config parse error: {0}")]
    Parse(#[from] serde_json::Error),
}

/// `%APPDATA%/goose-overlay/` (created if missing).
fn config_dir() -> Result<PathBuf, ConfigError> {
    let dir = dirs::config_dir()
        .ok_or(ConfigError::NoConfigDir)?
        .join("goose-overlay");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn config_path() -> Result<PathBuf, ConfigError> {
    Ok(config_dir()?.join("config.json"))
}

/// `%APPDATA%/goose-overlay/themes/` (created if missing) — user `.css` themes.
pub fn themes_dir() -> Result<PathBuf, ConfigError> {
    let dir = config_dir()?.join("themes");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Load config from disk, falling back to defaults if the file is missing.
/// A corrupt file is a hard error so we never silently discard user settings.
pub fn load() -> Result<Config, ConfigError> {
    let path = config_path()?;
    match fs::read_to_string(&path) {
        Ok(text) => {
            let config: Config = serde_json::from_str(&text)?;
            Ok(migrate_hotkeys(config, &text))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e.into()),
    }
}

/// Seed `hotkeys` from the legacy singular `hotkey` field when a pre-Round-2
/// config is loaded (Round-2 item 3). No-op once `hotkeys` is populated.
fn migrate_hotkeys(mut config: Config, raw: &str) -> Config {
    if config.hotkeys.is_empty() {
        let legacy = serde_json::from_str::<serde_json::Value>(raw)
            .ok()
            .and_then(|v| v.get("hotkey").and_then(|h| h.as_str()).map(String::from));
        config.hotkeys = vec![legacy.unwrap_or_else(|| "Alt+Space".to_string())];
    }
    config
}

/// Persist config to disk (pretty-printed for hand-inspection).
pub fn save(config: &Config) -> Result<(), ConfigError> {
    let path = config_path()?;
    let text = serde_json::to_string_pretty(config)?;
    fs::write(&path, text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.hotkeys, vec!["Alt+Space".to_string()]);
        assert!(!c.setup_completed);
        assert!(c.notifications.approval_needed);
    }

    #[test]
    fn legacy_hotkey_migrates_to_list() {
        // A pre-Round-2 config with a custom singular `hotkey` must carry over.
        let raw = r#"{"hotkey":"Control+Shift+K","theme":"dark"}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        assert!(cfg.hotkeys.is_empty()); // field default is empty before migration
        let cfg = migrate_hotkeys(cfg, raw);
        assert_eq!(cfg.hotkeys, vec!["Control+Shift+K".to_string()]);
    }

    #[test]
    fn missing_hotkey_migrates_to_default() {
        let raw = r#"{"theme":"dark"}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let cfg = migrate_hotkeys(cfg, raw);
        assert_eq!(cfg.hotkeys, vec!["Alt+Space".to_string()]);
    }

    #[test]
    fn config_roundtrips_through_json() {
        let c = Config {
            theme: "dark".to_string(),
            default_context_folder: Some("C:/Users/x/Documents/Goose".to_string()),
            ..Config::default()
        };
        let text = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back.theme, "dark");
        assert_eq!(
            back.default_context_folder.as_deref(),
            Some("C:/Users/x/Documents/Goose")
        );
    }

    #[test]
    fn partial_json_fills_defaults() {
        // A config written by an older build (only one field) must still load.
        let back: Config = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(back.theme, "dark");
        assert_eq!(back.ollama_base_url, "http://localhost:11434");
    }
}
