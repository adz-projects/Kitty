//! App configuration: a single JSON document at `%APPDATA%/Kitty/config.json`,
//! loaded at startup and written back on change. Stores **metadata only** —
//! never secrets (those live in the Windows Credential Manager via
//! `keyring`).

pub mod env_helper;
pub mod providers;
pub mod recipe_yaml;
pub mod recipes;
pub mod scheduled_tasks;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

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
    /// Accelerator that always opens a brand-new chat window with a fresh
    /// session (Feature 4/5 — multiple simultaneous chat windows). `None` =
    /// not registered. Distinct from `hotkeys` above, which toggles the
    /// overlay/focuses the one classic singleton main window.
    #[serde(default)]
    pub open_window_hotkey: Option<String>,
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
    /// Id of the active provider profile, if any.
    pub active_provider_id: Option<String>,
    /// Disable file/folder drop while a remote-tier provider is active.
    pub strict_remote_mode: bool,
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
    /// Whether the bundled `replacement-mcp` server (see
    /// `plugins/replacement-mcp/`) is registered+enabled as a BigTiny MCP
    /// server. **On** by default: its context-optimized shell/file/web/document
    /// tools are what makes the small local models Kitty targets usable at all,
    /// so shipping it off meant a default install had no working tool set until
    /// the user found this toggle. Kitty never spawns/monitors this process
    /// itself; BigTiny does, like any other MCP server — this flag only drives
    /// whether `bigtiny::mcp::ensure_builtin_servers`'s registration is enabled.
    #[serde(default = "default_true")]
    pub replacement_mcp_enabled: bool,
    /// One-shot marker for the `replacement_mcp_enabled` default flip above.
    /// The field is always serialized, so an install predating the flip has a
    /// literal `false` on disk that `#[serde(default)]` can never reach —
    /// indistinguishable from a deliberate opt-out. This marker resolves that:
    /// `migrate_replacement_mcp_enabled` force-enables exactly once per
    /// install, sets this, and never touches the flag again, so a user who
    /// turns it back off afterwards stays off.
    #[serde(default)]
    pub replacement_mcp_default_migrated: bool,
    /// Whether the bundled `wasm-math-mcp` server (see
    /// `plugins/wasm-math-mcp/`) is registered+enabled as a BigTiny MCP
    /// server. On by default — sandboxed Python/NumPy execution is safe and
    /// broadly useful for any model, unlike `replacement_mcp`'s wholesale
    /// tool-set swap. No credentials involved.
    #[serde(default = "default_true")]
    pub wasm_math_mcp_enabled: bool,
    /// Whether the bundled `brave-mcp-search` server (see
    /// `plugins/brave-mcp-search/`) is registered+enabled as a BigTiny MCP
    /// server. Off by default (requires a Brave Search API key, unlike
    /// `wasm_math_mcp_enabled`). The API key itself lives in the keyring
    /// (`config::providers::{set_secret,get_secret_async,delete_secret}`
    /// under the fixed id `"brave-mcp-search"`), never here — this flag only
    /// tracks user intent. Disabling this server always deletes the stored
    /// key (see `commands::set_brave_mcp_search_enabled`), so re-enabling it
    /// always requires re-entering the key — deliberate, not a bug: an old
    /// key silently reactivating without the user seeing it again would be
    /// surprising for a server that reaches an external paid API.
    #[serde(default)]
    pub brave_mcp_search_enabled: bool,
    /// User-defined scheduled tasks (instructions the agent runs later,
    /// one-shot or recurring, with or without the app open) — see
    /// `scheduled_tasks::ScheduledTask`.
    #[serde(default)]
    pub scheduled_tasks: Vec<ScheduledTask>,
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
    /// Command used to launch the BigTiny daemon — the bundled
    /// `bigtiny-daemon.exe` (see `plugins/build.py`, same `externalBin`
    /// convention as the other bundled plugins) if present, else `python`
    /// for dev convenience (paired with `bigtiny_args`'s `-m bigtiny`
    /// default, so `cargo tauri dev` still works from a source checkout).
    #[serde(default = "default_bigtiny_command")]
    pub bigtiny_command: String,
    /// Arguments before `--port`/`--host`. Empty for the bundled exe; `["-m",
    /// "bigtiny"]` for the dev-convenience `python` fallback above.
    #[serde(default = "default_bigtiny_args")]
    pub bigtiny_args: Vec<String>,
    /// Working directory to spawn BigTiny in — the checkout that contains the
    /// `bigtiny` package, when it isn't pip-installed into the interpreter
    /// and the bundled exe isn't in use. `None` = inherit Kitty's own cwd.
    #[serde(default)]
    pub bigtiny_dir: Option<String>,
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
            open_window_hotkey: None,
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
            replacement_mcp_enabled: default_true(),
            // A brand-new config needs no flip, so it starts already-migrated.
            replacement_mcp_default_migrated: true,
            wasm_math_mcp_enabled: default_true(),
            brave_mcp_search_enabled: false,
            scheduled_tasks: Vec::new(),
            ollama_enabled: default_true(),
            bigtiny_command: default_bigtiny_command(),
            bigtiny_args: default_bigtiny_args(),
            bigtiny_dir: None,
            recipes: recipes::builtin_templates(),
        }
    }
}

fn default_bigtiny_command() -> String {
    bundled_plugin_path("bigtiny-daemon.exe").unwrap_or_else(|| "python".to_string())
}

/// Empty when the bundled exe was found (it needs no `-m bigtiny` — that's
/// only how the dev-convenience bare `python` fallback locates the package).
fn default_bigtiny_args() -> Vec<String> {
    if bundled_plugin_path("bigtiny-daemon.exe").is_some() {
        Vec::new()
    } else {
        vec!["-m".to_string(), "bigtiny".to_string()]
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

fn default_adaptive_pathway_enabled() -> bool {
    true
}

/// The sidecar is bundled as a frozen `.exe` via Tauri's `externalBin`
/// mechanism (see `plugins/build.py`, `tauri.conf.json`'s `bundle.externalBin`)
/// and lands next to the main app executable at install time — no Python
/// runtime on the end user's machine. Resolves that path if present; falls
/// back to a bare PATH-relative name otherwise (dev convenience only: running
/// via `cargo run`/`tauri dev` never copies the bundled sidecar alongside the
/// dev binary, and a developer working on the sidecar itself can override
/// this config field to `uv run ...` regardless of this default).
fn default_ap_launch_command() -> String {
    bundled_plugin_path("adaptive-pathway-sidecar.exe")
        .unwrap_or_else(|| "adaptive-pathway-sidecar".to_string())
}

/// Resolves `<name>` next to the currently-running executable, if it exists.
/// Shared by every bundled-plugin default (also used to resolve the
/// replacement-mcp and adaptive-pathway-mcp exe paths for BigTiny's MCP
/// server registration — see `bigtiny::mcp::ensure_builtin_servers`).
pub(crate) fn bundled_plugin_path(name: &str) -> Option<String> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let candidate = dir.join(name);
    candidate
        .exists()
        .then(|| candidate.to_string_lossy().into_owned())
}

/// An absolute default, not a bare relative `./pathway.db` — the sidecar
/// (spawned by Kitty) and the adaptive-pathway MCP tools (spawned by
/// BigTiny) are separate processes that can each have a different working
/// directory, so a relative path risks each one silently resolving to a
/// *different* file even when configured "the same." An absolute path
/// removes that ambiguity. Consolidated under `%APPDATA%/Kitty/` rather than
/// this field's pre-consolidation `%LOCALAPPDATA%/adaptive-pathway/` location
/// (`old_default_ap_db_path`, which `migrate_ap_db_path` below migrates an
/// existing install's db file away from).
fn default_ap_db_path() -> String {
    config_dir()
        .map(|d| d.join("adaptive-pathway").join("pathway.db"))
        .unwrap_or_else(|_| old_default_ap_db_path_buf())
        .to_string_lossy()
        .replace('\\', "/")
}

/// The pre-consolidation default — kept only so `migrate_ap_db_path` can
/// recognize a config that still has it and move the db file over; also
/// `default_ap_db_path`'s own fallback if `%APPDATA%` can't be resolved for
/// some reason (best-effort, never panics, matching this function's own
/// prior fallback-to-relative-ish behavior).
fn old_default_ap_db_path_buf() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("adaptive-pathway")
        .join("pathway.db")
}

fn old_default_ap_db_path() -> String {
    old_default_ap_db_path_buf()
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

/// `%APPDATA%/Kitty/` (created if missing). One-time migration: if this dir
/// has no `config.json` yet but the pre-rename `%APPDATA%/goose-overlay/`
/// does, move the whole directory (bringing `config.json` and `themes/`
/// along) rather than leaving the user's settings stranded under the old name.
pub(crate) fn config_dir() -> Result<PathBuf, ConfigError> {
    let base = dirs::config_dir().ok_or(ConfigError::NoConfigDir)?;
    let dir = base.join("Kitty");
    if !dir.join("config.json").exists() {
        let old_dir = base.join("goose-overlay");
        if old_dir.exists() {
            migrate_dir(&old_dir, &dir);
        }
    }
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// BigTiny's own data root (its SQLite db, the directory-sandbox's
/// always-allowed cache dir, and its recipes dir — see `bigtiny/paths.py`'s
/// `data_dir()`) — consolidated under `%APPDATA%/Kitty/bigtiny/` rather than
/// BigTiny's own standalone-dev default of `~/.bigtiny`, which Kitty
/// overrides by setting `BIGTINY_DATA_DIR` when spawning it
/// (`lifecycle::bigtiny_proc::spawn`). One-time migration: if the new dir has
/// no `bigtiny.db` yet but `~/.bigtiny` does, move the whole directory over.
pub fn bigtiny_data_dir() -> Result<PathBuf, ConfigError> {
    let dir = config_dir()?.join("bigtiny");
    if !dir.join("bigtiny.db").exists() {
        if let Some(home) = dirs::home_dir() {
            let old_dir = home.join(".bigtiny");
            if old_dir.exists() {
                migrate_dir(&old_dir, &dir);
            }
        }
    }
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Best-effort directory migration: a same-volume rename is instant and
/// atomic; if that fails (cross-volume, or `new` already exists as some
/// stray empty dir), fall back to a recursive copy-then-remove so the old
/// files are still present in the new location and nothing is silently lost.
/// Shared by every "consolidate a scattered data dir under %APPDATA%/Kitty/"
/// migration (config dir, BigTiny's data root, ...).
fn migrate_dir(old: &Path, new: &Path) {
    if fs::rename(old, new).is_ok() {
        tracing::info!("migrated data dir {} -> {}", old.display(), new.display());
        return;
    }
    if copy_dir_recursive(old, new).is_ok() {
        let _ = fs::remove_dir_all(old);
        tracing::info!(
            "migrated data dir {} -> {} (copy fallback)",
            old.display(),
            new.display()
        );
    } else {
        tracing::warn!(
            "could not migrate data dir {} -> {}",
            old.display(),
            new.display()
        );
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else {
            fs::copy(entry.path(), dst_path)?;
        }
    }
    Ok(())
}

fn config_path() -> Result<PathBuf, ConfigError> {
    Ok(config_dir()?.join("config.json"))
}

/// `%APPDATA%/Kitty/themes/` (created if missing) — user `.css` themes.
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
            Ok(migrate_replacement_mcp_enabled(migrate_ap_db_path(
                migrate_bigtiny_launch_command(migrate_ap_launch_command(migrate_recipes(
                    migrate_hotkeys(config, &text),
                ))),
            )))
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

/// True for a configured launch command that looks like a filesystem path
/// (contains a path separator, so it's a specific resolved location, not a
/// bare PATH-resolved command name like `python`/`uv`) which is either
/// missing entirely or present-but-empty. A zero-byte file matters because
/// that's exactly what a committed `externalBin` placeholder looks like
/// (`src-tauri/binaries/README.md`) — a `cargo build`/`check` re-copies
/// whatever's in `src-tauri/binaries/` into `target/debug/`, so resetting
/// those placeholders (e.g. right before a commit) silently zeroes out a
/// previously-working absolute dev path the config still points at. Deliberate
/// PATH-based overrides (`uv`, `python`, no separator) are never touched here —
/// only a specific resolved path can go stale like this.
fn command_path_is_stale(command: &str) -> bool {
    if !(command.contains('/') || command.contains('\\')) {
        return false;
    }
    match fs::metadata(command) {
        Ok(meta) => meta.len() == 0,
        Err(_) => true,
    }
}

/// Self-heals an existing install's `adaptive_pathway_launch_command` onto
/// the bundled sidecar path, mirroring the AP env-var self-heal already done
/// in `lifecycle::start_stack`. `#[serde(default = "default_ap_launch_command")]`
/// only ever runs when the field is *absent* from the loaded JSON — a config
/// saved before the bundled-path resolution existed already has the old bare
/// literal `"adaptive-pathway-sidecar"` stored explicitly, so that improved
/// default never gets a chance to apply on its own. Also self-heals a
/// previously-resolved absolute path that's since gone missing or been
/// reset to an empty placeholder (`command_path_is_stale`) — a value the
/// user (or a prior dev-mode override) deliberately set to a bare PATH
/// command, e.g. `uv run ...`, is left untouched either way.
fn migrate_ap_launch_command(mut config: Config) -> Config {
    const OLD_BARE_DEFAULT: &str = "adaptive-pathway-sidecar";
    let stale = config.adaptive_pathway_launch_command == OLD_BARE_DEFAULT
        || command_path_is_stale(&config.adaptive_pathway_launch_command);
    if stale {
        if let Some(bundled) = bundled_plugin_path("adaptive-pathway-sidecar.exe") {
            config.adaptive_pathway_launch_command = bundled;
        }
    }
    config
}

/// Self-heals an existing install's `bigtiny_command`/`bigtiny_args` from the
/// pre-bundling dev default (`python -m bigtiny`) onto the bundled exe, once
/// one is present — same rationale as `migrate_ap_launch_command`, including
/// the same stale-absolute-path self-heal (`command_path_is_stale`). A
/// deliberate override (e.g. `uv run bigtiny`, or a source checkout via
/// `bigtiny_dir`) is left untouched.
fn migrate_bigtiny_launch_command(mut config: Config) -> Config {
    const OLD_COMMAND: &str = "python";
    let old_args = ["-m".to_string(), "bigtiny".to_string()];
    let stale = (config.bigtiny_command == OLD_COMMAND && config.bigtiny_args == old_args)
        || command_path_is_stale(&config.bigtiny_command);
    if stale {
        if let Some(bundled) = bundled_plugin_path("bigtiny-daemon.exe") {
            config.bigtiny_command = bundled;
            config.bigtiny_args = Vec::new();
        }
    }
    config
}

/// Self-heals an existing install's `adaptive_pathway_db_path` off its
/// pre-consolidation location (`%LOCALAPPDATA%/adaptive-pathway/pathway.db`)
/// onto the new one under `%APPDATA%/Kitty/adaptive-pathway/` — physically
/// moves the db file if one is sitting at the old location and nothing's at
/// the new one yet, same rename-then-copy-fallback as `migrate_dir`, then
/// repoints the config field. Only migrates the exact literal the old
/// default used to produce; a deliberate override (or an install that
/// already has the new default) is left untouched. Thin wrapper around
/// `migrate_ap_db_path_impl` so the real move logic is unit-testable against
/// fake temp-dir paths instead of this machine's real
/// `%LOCALAPPDATA%/adaptive-pathway/pathway.db` (which may hold real data).
fn migrate_ap_db_path(config: Config) -> Config {
    migrate_ap_db_path_impl(config, &old_default_ap_db_path(), &default_ap_db_path())
}

fn migrate_ap_db_path_impl(mut config: Config, old_default: &str, new_default: &str) -> Config {
    if config.adaptive_pathway_db_path != old_default {
        return config;
    }
    let old = PathBuf::from(old_default);
    let new = PathBuf::from(new_default);
    if old.exists() && !new.exists() {
        if let Some(parent) = new.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if fs::rename(&old, &new).is_err() && fs::copy(&old, &new).is_ok() {
            let _ = fs::remove_file(&old);
        }
    }
    config.adaptive_pathway_db_path = new_default.to_string();
    config
}

/// Seed `hotkeys` from the legacy singular `hotkey` field when a pre-Round-2
/// config is loaded (Round-2 item 3). No-op once `hotkeys` is populated.
/// One-time flip of `replacement_mcp_enabled` to its new `true` default for
/// installs created while it defaulted to `false`. Because the field is always
/// serialized, serde's default never applies to an existing config — the stored
/// `false` would otherwise stick forever, leaving upgraded installs with no
/// usable tool set. Guarded by `replacement_mcp_default_migrated` so this runs
/// exactly once: a user who deliberately turns the server off after the flip
/// keeps it off across every later launch.
///
/// Note this only mutates the in-memory config; it's persisted by the next
/// `save`, and re-applied harmlessly on every launch until then (the flag it
/// sets is itself part of what gets saved).
fn migrate_replacement_mcp_enabled(mut config: Config) -> Config {
    if !config.replacement_mcp_default_migrated {
        config.replacement_mcp_enabled = true;
        config.replacement_mcp_default_migrated = true;
    }
    config
}

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

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kitty-config-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir
    }

    #[test]
    fn migrate_dir_moves_files_via_rename() {
        let old = temp_dir("rename-old");
        let new = temp_dir("rename-new");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("config.json"), "{}").unwrap();
        fs::create_dir_all(old.join("themes")).unwrap();
        fs::write(old.join("themes").join("dark.css"), "body{}").unwrap();

        migrate_dir(&old, &new);

        assert!(!old.exists());
        assert!(new.join("config.json").exists());
        assert!(new.join("themes").join("dark.css").exists());
        let _ = fs::remove_dir_all(&new);
    }

    #[test]
    fn copy_dir_recursive_copies_nested_files() {
        let old = temp_dir("copy-old");
        let new = temp_dir("copy-new");
        fs::create_dir_all(old.join("themes")).unwrap();
        fs::write(old.join("config.json"), "{\"theme\":\"dark\"}").unwrap();
        fs::write(old.join("themes").join("x.css"), "x").unwrap();

        copy_dir_recursive(&old, &new).unwrap();

        assert_eq!(
            fs::read_to_string(new.join("config.json")).unwrap(),
            "{\"theme\":\"dark\"}"
        );
        assert_eq!(fs::read_to_string(new.join("themes").join("x.css")).unwrap(), "x");
        // Source untouched — the caller decides whether to remove it.
        assert!(old.exists());
        let _ = fs::remove_dir_all(&old);
        let _ = fs::remove_dir_all(&new);
    }

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
        // Predates the second (open-new-chat-window) hotkey entirely — must
        // default to unregistered, not error out on the missing field.
        assert_eq!(back.open_window_hotkey, None);
    }

    #[test]
    fn open_window_hotkey_roundtrips_through_json() {
        let c = Config {
            open_window_hotkey: Some("Ctrl+Alt+N".to_string()),
            ..Config::default()
        };
        let text = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back.open_window_hotkey.as_deref(), Some("Ctrl+Alt+N"));
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
    fn migrate_ap_launch_command_leaves_custom_override_untouched() {
        // A deliberate dev-mode override (e.g. pointing at `uv run ...`) must
        // never be silently overwritten by the bundled-path self-heal.
        let mut cfg = Config::default();
        cfg.adaptive_pathway_launch_command = "uv run adaptive-pathway-sidecar".to_string();
        let migrated = migrate_ap_launch_command(cfg);
        assert_eq!(
            migrated.adaptive_pathway_launch_command,
            "uv run adaptive-pathway-sidecar"
        );
    }

    #[test]
    fn migrate_ap_launch_command_is_a_noop_with_no_bundled_binary_present() {
        // The test binary has no `adaptive-pathway-sidecar.exe` sitting next to
        // it, so `bundled_plugin_path` finds nothing and the old bare-name
        // literal is left as-is — this is also the realistic behavior for a
        // `cargo run`/`tauri dev` build with no frozen binary in place yet.
        let cfg = Config {
            adaptive_pathway_launch_command: "adaptive-pathway-sidecar".to_string(),
            ..Config::default()
        };
        let migrated = migrate_ap_launch_command(cfg);
        assert_eq!(
            migrated.adaptive_pathway_launch_command,
            "adaptive-pathway-sidecar"
        );
    }

    #[test]
    fn migrate_bigtiny_launch_command_leaves_custom_override_untouched() {
        let cfg = Config {
            bigtiny_command: "uv".to_string(),
            bigtiny_args: vec!["run".to_string(), "bigtiny".to_string()],
            ..Config::default()
        };
        let migrated = migrate_bigtiny_launch_command(cfg);
        assert_eq!(migrated.bigtiny_command, "uv");
        assert_eq!(migrated.bigtiny_args, vec!["run", "bigtiny"]);
    }

    #[test]
    fn migrate_bigtiny_launch_command_is_a_noop_with_no_bundled_binary_present() {
        // The test binary has no `bigtiny-daemon.exe` sitting next to it, so
        // `bundled_plugin_path` finds nothing and the old dev-default
        // command/args pair is left as-is.
        let cfg = Config {
            bigtiny_command: "python".to_string(),
            bigtiny_args: vec!["-m".to_string(), "bigtiny".to_string()],
            ..Config::default()
        };
        let migrated = migrate_bigtiny_launch_command(cfg);
        assert_eq!(migrated.bigtiny_command, "python");
        assert_eq!(migrated.bigtiny_args, vec!["-m", "bigtiny"]);
    }

    #[test]
    fn command_path_is_stale_true_for_missing_absolute_path() {
        assert!(command_path_is_stale(
            "C:/nonexistent/definitely/not/here/bigtiny-daemon.exe"
        ));
    }

    #[test]
    fn command_path_is_stale_true_for_empty_file() {
        // Exactly what a committed externalBin placeholder looks like
        // (src-tauri/binaries/README.md) — a real bug this session hit:
        // resetting placeholders + `cargo check` zeroed out a previously
        // real `target/debug/bigtiny-daemon.exe` a live config still pointed
        // at, silently breaking BigTiny startup.
        let dir = std::env::temp_dir().join(format!("kitty-stale-test-{}", uuid_like()));
        fs::create_dir_all(&dir).unwrap();
        let empty = dir.join("bigtiny-daemon.exe");
        fs::write(&empty, []).unwrap();
        assert!(command_path_is_stale(empty.to_str().unwrap()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn command_path_is_stale_false_for_a_real_nonempty_file() {
        let dir = std::env::temp_dir().join(format!("kitty-stale-test-{}", uuid_like()));
        fs::create_dir_all(&dir).unwrap();
        let real = dir.join("bigtiny-daemon.exe");
        fs::write(&real, b"not actually a PE, just non-empty").unwrap();
        assert!(!command_path_is_stale(real.to_str().unwrap()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn command_path_is_stale_false_for_bare_path_command_names() {
        // No path separator -> a PATH-resolved command (`python`, `uv`), never
        // treated as a stale resolved path even though it "doesn't exist" as
        // a literal file in the current directory.
        assert!(!command_path_is_stale("python"));
        assert!(!command_path_is_stale("uv"));
    }

    #[test]
    fn migrate_bigtiny_launch_command_without_a_bundled_sibling_leaves_stale_path_as_is() {
        // `bundled_plugin_path` resolves next to the *test binary*, which has
        // no `bigtiny-daemon.exe` sibling, so even a detected-as-stale path
        // is left in place rather than cleared to nothing — matches the
        // realistic dev-build case (no frozen binary present yet). The
        // real-world healing path (a bundled sibling IS present) is covered
        // by `bundled_plugin_path`'s own existence check plus
        // `command_path_is_stale`'s tests above; faking `current_exe()`
        // itself isn't practical from a unit test.
        let dir = std::env::temp_dir().join(format!("kitty-stale-test-{}", uuid_like()));
        fs::create_dir_all(&dir).unwrap();
        let stale_daemon = dir.join("bigtiny-daemon.exe");
        fs::write(&stale_daemon, []).unwrap(); // zero-byte, like a reset placeholder

        let cfg = Config {
            bigtiny_command: stale_daemon.to_str().unwrap().to_string(),
            bigtiny_args: vec!["--some-flag".to_string()],
            ..Config::default()
        };
        let migrated = migrate_bigtiny_launch_command(cfg);
        assert_eq!(migrated.bigtiny_command, stale_daemon.to_str().unwrap());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrate_ap_db_path_leaves_custom_override_untouched() {
        let cfg = Config {
            adaptive_pathway_db_path: "D:/somewhere/custom/pathway.db".to_string(),
            ..Config::default()
        };
        let migrated = migrate_ap_db_path(cfg);
        assert_eq!(
            migrated.adaptive_pathway_db_path,
            "D:/somewhere/custom/pathway.db"
        );
    }

    #[test]
    fn migrate_ap_db_path_impl_moves_the_file_when_old_exists_and_new_does_not() {
        let dir = std::env::temp_dir().join(format!("kitty-ap-db-test-{}", uuid_like()));
        let old = dir.join("old").join("pathway.db");
        let new = dir.join("new").join("pathway.db");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::write(&old, b"real learned data").unwrap();

        let cfg = Config {
            adaptive_pathway_db_path: old.to_str().unwrap().to_string(),
            ..Config::default()
        };
        let migrated =
            migrate_ap_db_path_impl(cfg, old.to_str().unwrap(), new.to_str().unwrap());

        assert_eq!(migrated.adaptive_pathway_db_path, new.to_str().unwrap());
        assert!(!old.exists());
        assert_eq!(fs::read(&new).unwrap(), b"real learned data");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrate_ap_db_path_impl_repoints_even_when_no_old_file_exists() {
        // Fresh install / already-migrated case: nothing to move, but the
        // config field should still land on the new default rather than
        // staying pinned to the recognized old literal.
        let dir = std::env::temp_dir().join(format!("kitty-ap-db-test-{}", uuid_like()));
        let old = dir.join("old").join("pathway.db");
        let new = dir.join("new").join("pathway.db");

        let cfg = Config {
            adaptive_pathway_db_path: old.to_str().unwrap().to_string(),
            ..Config::default()
        };
        let migrated =
            migrate_ap_db_path_impl(cfg, old.to_str().unwrap(), new.to_str().unwrap());

        assert_eq!(migrated.adaptive_pathway_db_path, new.to_str().unwrap());
        assert!(!new.exists());
    }

    #[test]
    fn migrate_ap_db_path_impl_never_overwrites_an_existing_new_file() {
        let dir = std::env::temp_dir().join(format!("kitty-ap-db-test-{}", uuid_like()));
        let old = dir.join("old").join("pathway.db");
        let new = dir.join("new").join("pathway.db");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::create_dir_all(new.parent().unwrap()).unwrap();
        fs::write(&old, b"stale copy").unwrap();
        fs::write(&new, b"already-migrated real data").unwrap();

        let cfg = Config {
            adaptive_pathway_db_path: old.to_str().unwrap().to_string(),
            ..Config::default()
        };
        let _ = migrate_ap_db_path_impl(cfg, old.to_str().unwrap(), new.to_str().unwrap());

        assert_eq!(fs::read(&new).unwrap(), b"already-migrated real data");

        fs::remove_dir_all(&dir).unwrap();
    }

    fn uuid_like() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        format!(
            "{}-{:?}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
            std::thread::current().id()
        )
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
    fn replacement_mcp_enables_once_for_a_config_predating_the_default_flip() {
        // The pre-flip shape: an explicit `false` with no migration marker.
        // Serde's new `default_true` can't reach an explicitly-present field,
        // so only the migration gets this install a working tool set.
        let cfg: Config =
            serde_json::from_str(r#"{"replacement_mcp_enabled":false}"#).unwrap();
        assert!(!cfg.replacement_mcp_enabled);
        assert!(!cfg.replacement_mcp_default_migrated);

        let migrated = migrate_replacement_mcp_enabled(cfg);
        assert!(migrated.replacement_mcp_enabled);
        assert!(migrated.replacement_mcp_default_migrated);
    }

    #[test]
    fn replacement_mcp_respects_an_opt_out_made_after_the_flip() {
        // Once the marker is set, a user turning the server back off must
        // stick — otherwise every launch would silently re-enable it.
        let cfg: Config = serde_json::from_str(
            r#"{"replacement_mcp_enabled":false,"replacement_mcp_default_migrated":true}"#,
        )
        .unwrap();
        let migrated = migrate_replacement_mcp_enabled(cfg);
        assert!(!migrated.replacement_mcp_enabled);
    }

    #[test]
    fn replacement_mcp_is_on_for_a_brand_new_config() {
        let cfg = Config::default();
        assert!(cfg.replacement_mcp_enabled);
        assert!(cfg.replacement_mcp_default_migrated);
    }

    #[test]
    fn old_shape_config_migrates_wizard_redesign_defaults() {
        // A config predating the wizard redesign (ollama_enabled) must still
        // load, with Ollama left enabled (pre-existing installs already have
        // it configured).
        let back: Config = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert!(back.ollama_enabled);
    }
}
