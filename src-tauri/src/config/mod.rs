//! App configuration: a single JSON document at
//! `%APPDATA%/goose-overlay/config.json`, loaded at startup and written back on
//! change. Stores **metadata only** — never secrets (those live in the Windows
//! Credential Manager via `keyring`, wired in Phase 5).

pub mod env_helper;
pub mod providers;
pub mod recipe_yaml;
pub mod recipes;
pub mod scheduled_tasks;

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use providers::ProviderProfile;
use recipes::Recipe;
use scheduled_tasks::ScheduledTask;

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
    /// Maps a goosed session id → `"chat"` | `"agentic"`. Absent = default to
    /// chat (owner ask, later round — providers no longer carry their own
    /// `tools_enabled` default either; the per-session toggle is now the only
    /// mode selector).
    /// Persisted (not transient) so resuming a flipped session doesn't
    /// silently revert its attachment/tool semantics — mirrors
    /// `session_folders` above.
    #[serde(default)]
    pub session_modes: HashMap<String, String>,
    /// Whether Kitty should spawn/supervise the Adaptive Pathway extension's
    /// HTTP sidecar (and register its Goose extension in lockstep — see
    /// Settings → Advanced → Adaptive Pathway's single enable checkbox). On by
    /// default for fresh installs; if the Python package isn't installed the
    /// sidecar just reports `Down` (graceful degradation, no error spam).
    #[serde(default = "default_adaptive_pathway_enabled")]
    pub adaptive_pathway_enabled: bool,
    /// Command used to launch the sidecar (default assumes it's on PATH).
    #[serde(default = "default_ap_launch_command")]
    pub adaptive_pathway_launch_command: String,
    /// Extra arguments appended before `--db-path`/`--port` (e.g. `--config-path`).
    #[serde(default)]
    pub adaptive_pathway_launch_args: Vec<String>,
    /// SQLite DB path passed to the sidecar via `--db-path`.
    #[serde(default = "default_ap_db_path")]
    pub adaptive_pathway_db_path: String,
    /// Port the sidecar binds to (matches `run_server`'s own literal default).
    #[serde(default = "default_ap_port")]
    pub adaptive_pathway_port: u16,
    /// Ollama model tag used for adaptive-pathway's context embeddings.
    /// Passed to both Python processes (the sidecar and the goosed-spawned
    /// MCP extension) via `AP_EMBED_OLLAMA_MODEL` so they can't drift apart —
    /// cross-compatibility requires every user's vectors to live in the same
    /// embedding space, so this is a single pinned tag, not user-configurable
    /// per-provider.
    #[serde(default = "default_ap_embedding_model")]
    pub adaptive_pathway_embedding_model: String,
    /// User-defined scheduled tasks (instructions the agent runs later,
    /// one-shot or recurring, with or without the app open) — see
    /// `scheduled_tasks::ScheduledTask`.
    #[serde(default)]
    pub scheduled_tasks: Vec<ScheduledTask>,
    /// Absolute path to `goose.exe`, set automatically once the wizard's
    /// one-click Goose install (download + extract the CLI zip) completes, or
    /// manually via the wizard's "point at an existing install" fallback.
    /// Checked first by `lifecycle::goosed::locate_goose`, before the
    /// `GOOSE_BIN` env var / Goose Desktop bundle path / bare `goose` on PATH.
    #[serde(default)]
    pub goose_binary_override: Option<String>,
    /// Whether local inference (Ollama) is in play at all for this install.
    /// Set explicitly by the wizard's first-screen fork ("Run on this
    /// computer" vs. "Use my own API key") and toggleable later from
    /// Settings → Advanced. Defaults `true` so pre-existing installs (which
    /// predate this field and already have Ollama configured) are unaffected.
    /// When `false`: the Ollama Models settings section and the "Ollama"
    /// provider-type option are hidden, and `start_stack`/`compute_status`
    /// stop trying to reach Ollama at all (see
    /// `lifecycle::ollama_proc::requires_local_ollama`).
    #[serde(default = "default_true")]
    pub ollama_enabled: bool,
    /// User + built-in recipe templates (Goose recipes reinterpreted as
    /// client-side chat-turn templates — see `recipes` module doc comment).
    /// Seeded with the 4 built-ins two ways: `Config::default()` below contains
    /// them (so a missing key fills from the container-level `#[serde(default)]`,
    /// and the first-launch/corrupt-fallback paths that use `Config::default()`
    /// directly get them too), and `migrate_recipes` in `load` re-seeds the one
    /// case `Default` can't reach — an explicit `"recipes": []`.
    pub recipes: Vec<Recipe>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkeys: vec!["Alt+Space".to_string()],
            clipboard_hotkey: Some("Ctrl+Alt+Space".to_string()),
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
            adaptive_pathway_enabled: default_adaptive_pathway_enabled(),
            adaptive_pathway_launch_command: default_ap_launch_command(),
            adaptive_pathway_launch_args: Vec::new(),
            adaptive_pathway_db_path: default_ap_db_path(),
            adaptive_pathway_port: default_ap_port(),
            adaptive_pathway_embedding_model: default_ap_embedding_model(),
            scheduled_tasks: Vec::new(),
            goose_binary_override: None,
            ollama_enabled: default_true(),
            recipes: recipes::builtin_templates(),
        }
    }
}

fn default_true() -> bool {
    true
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

fn default_adaptive_pathway_enabled() -> bool {
    true
}

fn default_ap_launch_command() -> String {
    "adaptive-pathway-sidecar".to_string()
}

/// An absolute default, not a bare relative `./pathway.db` — the sidecar
/// (spawned by Kitty) and the Goose MCP extension (spawned by goosed, itself
/// spawned by Kitty) are three separate processes that can each have a
/// different working directory, so a relative path risks each one silently
/// resolving to a *different* file even when configured "the same." An
/// absolute path removes that ambiguity.
fn default_ap_db_path() -> String {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("adaptive-pathway")
        .join("pathway.db")
        .to_string_lossy()
        .replace('\\', "/")
}

fn default_ap_port() -> u16 {
    8700
}

fn default_ap_embedding_model() -> String {
    "qwen3-embedding:0.6b".to_string()
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
            Ok(migrate_recipes(migrate_hotkeys(config, &text)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e.into()),
    }
}

/// Re-seed the built-in recipe templates if a loaded config somehow has an
/// empty recipe list. The common cases are already covered by `Config`'s
/// `Default` (which contains the built-ins): a config predating the feature is
/// missing the `recipes` key entirely, so serde's container-level
/// `#[serde(default)]` fills it from `Default` (built-ins present → this is a
/// no-op), and first-launch/corrupt-fallback both use `Config::default()`
/// directly without going through `load`. This only additionally handles the
/// one gap `Default` can't: a config saved with an explicit `"recipes": []`.
/// Built-ins are never deletable (`commands::recipes` guards this), so a
/// populated list is never legitimately empty — re-seeding an empty one can't
/// clobber user intent.
fn migrate_recipes(mut config: Config) -> Config {
    if config.recipes.is_empty() {
        config.recipes = recipes::builtin_templates();
    }
    config
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
///
/// Written atomically: serialize to a sibling temp file, then rename over the
/// target. On Windows `fs::rename` maps to `MoveFileEx` with
/// `MOVEFILE_REPLACE_EXISTING`, so the swap is atomic — a crash mid-write leaves
/// either the old or the new complete file, never a truncated one. This matters
/// because `load` treats a corrupt config as a hard error (it never silently
/// discards user settings), so a torn write would otherwise brick startup.
pub fn save(config: &Config) -> Result<(), ConfigError> {
    let path = config_path()?;
    let text = serde_json::to_string_pretty(config)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, &path)?;
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

    #[test]
    fn old_shape_config_migrates_adaptive_pathway_defaults() {
        // A config predating the Adaptive Pathway integration must still load,
        // on by default (UX-simplification decision — enabled by default for
        // fresh/pre-existing-field configs alike), with sane
        // launch-command/db-path/port defaults.
        let back: Config = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert!(back.adaptive_pathway_enabled);
        assert_eq!(
            back.adaptive_pathway_launch_command,
            "adaptive-pathway-sidecar"
        );
        // Absolute (not a bare relative "./pathway.db" — see default_ap_db_path's
        // doc comment for why that's load-bearing, not cosmetic).
        assert!(back.adaptive_pathway_db_path.ends_with("pathway.db"));
        assert!(std::path::Path::new(&back.adaptive_pathway_db_path).is_absolute());
        assert_eq!(back.adaptive_pathway_port, 8700);
    }

    #[test]
    fn old_shape_config_migrates_embedding_model_default() {
        // A config predating the embedding-model requirement must still load,
        // defaulting to the one pinned cross-compatible tag every user shares.
        let back: Config = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(
            back.adaptive_pathway_embedding_model,
            "qwen3-embedding:0.6b"
        );
    }

    #[test]
    fn old_shape_config_migrates_scheduled_tasks_default() {
        // A config predating scheduled tasks must still load with an empty list.
        let back: Config = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert!(back.scheduled_tasks.is_empty());
    }

    #[test]
    fn old_shape_config_migrates_recipes_default() {
        // A config predating recipes must still load, seeded with the 4
        // built-in templates (unlike scheduled_tasks, which is correctly
        // empty for everyone) — the field has no override, so container-level
        // `#[serde(default)]` fills it from `Config::default()`.
        let back: Config = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(back.recipes.len(), 4);
        assert!(back.recipes.iter().all(|r| r.is_builtin));
        let slugs: Vec<_> = back.recipes.iter().map(|r| r.slug.as_str()).collect();
        assert!(slugs.contains(&"annotated_bibliography"));
    }

    #[test]
    fn migrate_recipes_reseeds_an_explicitly_empty_list() {
        // The one case `Default` can't reach: a config with `"recipes": []`
        // present (so serde uses the empty array, not the default).
        let mut cfg: Config = serde_json::from_str(r#"{"recipes":[]}"#).unwrap();
        assert!(cfg.recipes.is_empty());
        cfg = migrate_recipes(cfg);
        assert_eq!(cfg.recipes.len(), 4);
        assert!(cfg.recipes.iter().all(|r| r.is_builtin));
    }

    #[test]
    fn old_shape_config_migrates_wizard_redesign_defaults() {
        // A config predating the wizard redesign (goose_binary_override,
        // ollama_enabled) must still load: no override path, and Ollama left
        // enabled (pre-existing installs already have it configured).
        let back: Config = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(back.goose_binary_override, None);
        assert!(back.ollama_enabled);
    }
}
