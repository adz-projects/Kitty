//! App configuration: a single JSON document at
//! `%APPDATA%/goose-overlay/config.json`, loaded at startup and written back on
//! change. Stores **metadata only** — never secrets (those live in the Windows
//! Credential Manager via `keyring`, wired in Phase 5).

pub mod env_helper;
pub mod providers;

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
    /// Accelerator used for the global toggle shortcut (Phase 6 adds Copilot key).
    pub hotkey: String,
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
    /// Auto-summarize threshold (Goose setting; app-side until wired).
    pub auto_summarize_threshold: Option<u32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: "Alt+Space".to_string(),
            use_copilot_key: false,
            default_context_folder: None,
            ollama_base_url: "http://localhost:11434".to_string(),
            setup_completed: false,
            theme: "default".to_string(),
            background_image: None,
            background_dim: 0.3,
            notifications: NotificationPrefs::default(),
            remember_overlay_position: true,
            providers: Vec::new(),
            active_provider_id: None,
            strict_remote_mode: false,
            auto_summarize_threshold: None,
        }
    }
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
        Ok(text) => Ok(serde_json::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e.into()),
    }
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
        assert_eq!(c.hotkey, "Alt+Space");
        assert!(!c.setup_completed);
        assert!(c.notifications.approval_needed);
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
        assert_eq!(back.hotkey, "Alt+Space");
    }
}
