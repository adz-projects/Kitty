//! App configuration: a single JSON document at `%APPDATA%/Kitty/config.json`,
//! loaded at startup and written back on change. Stores **metadata only** —
//! never secrets (those live in the Windows Credential Manager via
//! `keyring`).

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
    // No `ollama_base_url` here on purpose. It survived the managed-Ollama
    // retirement (Phase 2b) as a settings field with no reader anywhere —
    // Kitty spawns no inference process, and a remote-Ollama provider profile
    // carries its own `base_url`. Struct-level `#[serde(default)]` and the
    // absence of `deny_unknown_fields` mean an existing config.json still
    // carrying the key loads fine; the key is simply ignored and drops out on
    // the next save.
    /// First-run wizard completion flag (gates the wizard in Phase 7).
    pub setup_completed: bool,
    /// Active theme name (built-ins `light`/`dark`, or a user `.css` filename).
    /// `"default"` is also accepted on load as `light`'s pre-rename id — see
    /// `migrate_theme_default_to_light`.
    pub theme: String,
    // Background-image machinery (path/dim/fit/position) removed
    // (release-fixes item 18) — same drop-silently-on-next-save story as
    // `ollama_base_url` above: no `deny_unknown_fields`, so an existing
    // config.json still carrying these keys loads fine and they're simply
    // ignored.
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
    /// Per-session reasoning-effort choice (item: thinking effort). Maps a
    /// session id → the wire value (`"off"`/`"low"`/`"medium"`/`"high"`) last
    /// chosen for it. Persisted so resuming a session keeps its effort;
    /// re-derived against the *active provider's* capability each time it's
    /// read (see `bigtiny::effort`), so a session that later points at a
    /// provider with no effort control just stops offering the dropdown.
    #[serde(default)]
    pub session_efforts: HashMap<String, String>,
    /// Whether the in-process behavioral-memory (pathway) engine, linked
    /// directly into the BigTiny daemon, is active for this install. On by
    /// default for fresh installs. Also gates whether Ollama must run at all
    /// (see `lifecycle::stack_needs_ollama`) — the engine's embeddings are
    /// always local-Ollama, regardless of the active chat provider.
    #[serde(default = "default_adaptive_pathway_enabled")]
    pub adaptive_pathway_enabled: bool,
    /// Ollama model tag used for the pathway engine's belief embeddings.
    /// Passed to the BigTiny daemon via `AP_EMBED_OLLAMA_MODEL` (see
    /// `lifecycle::bigtiny_proc::spawn`) so the daemon's `EmbeddingProvider`
    /// and Kitty's own embedding-model-presence checks (Settings, the health
    /// loop) stay pointed at the same tag.
    #[serde(default = "default_ap_embedding_model")]
    pub adaptive_pathway_embedding_model: String,
    /// Retired: `replacement-mcp` no longer exists as its own process — all
    /// 18 of its tools now live inside `kitty-tools` (see
    /// `kitty_tools_enabled` below), and `plugins/replacement-mcp/lean_mcp.py`
    /// stays in-tree, unbuilt, only as an oracle for re-verifying the Rust
    /// port against if a behavioral gap ever surfaces. This field is kept
    /// (not removed) purely as the source value `migrate_kitty_split_enabled`
    /// carries forward into `kitty_tools_enabled` on an existing install's
    /// first load after the split — removing it would break that migration
    /// for anyone who hasn't upgraded past it yet.
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
    /// server. **Retired** — superseded by `kitty-wasm` (see
    /// `kitty_wasm_enabled`). Kept purely as the one-shot migration source for
    /// `migrate_kitty_wasm_enabled` so an existing install that had math
    /// execution disabled doesn't suddenly gain it.
    #[serde(default = "default_true")]
    pub wasm_math_mcp_enabled: bool,
    /// Whether the bundled `kitty-wasm` server (see `plugins/kitty-wasm/`) is
    /// registered+enabled as a BigTiny MCP server — the Rust replacement for
    /// the retired `wasm-math-mcp` Python plugin. On by default: sandboxed
    /// WebAssembly (wasmtime + WASI) Python/arbitrary-module execution with
    /// no network and no filesystem beyond explicit mounts is safe and
    /// broadly useful. Its 26 MB CPython guest is bundled as an app resource
    /// so first use is offline. On first load after the cutover,
    /// `migrate_kitty_wasm_enabled` carries `wasm_math_mcp_enabled`'s value
    /// forward so a user who'd disabled the old server doesn't silently
    /// regain it.
    #[serde(default = "default_true")]
    pub kitty_wasm_enabled: bool,
    /// One-shot marker for the `wasm-math-mcp` -> `kitty-wasm` cutover, so a
    /// user who later flips `kitty_wasm_enabled` independently keeps that
    /// choice.
    #[serde(default)]
    pub kitty_wasm_default_migrated: bool,
    /// Whether the `brave_mcp_search` tool is advertised by the combined
    /// `kitty-tools` server (see `kitty_tools_enabled` below and
    /// `plugins/kitty-tools/src/tools/search.rs`) — no longer its own
    /// process (`brave-mcp-search` is retired). Off by default (requires a
    /// Brave Search API key, unlike `wasm_math_mcp_enabled`). The API key
    /// itself lives in the keyring
    /// (`config::providers::{set_secret,get_secret_async,delete_secret}`
    /// under the fixed id `"brave-mcp-search"`), never here — this flag only
    /// tracks user intent, and `bigtiny::mcp::ensure_builtin_servers` turns
    /// it (plus key presence) into kitty-tools's `BRAVE_API_KEY` env var.
    /// Disabling always deletes the stored key (see
    /// `commands::set_brave_mcp_search_enabled`), so re-enabling always
    /// requires re-entering the key — deliberate, not a bug: an old key
    /// silently reactivating without the user seeing it again would be
    /// surprising for a tool that reaches an external paid API.
    #[serde(default)]
    pub brave_mcp_search_enabled: bool,
    /// Whether `generate_accessible_table`/`generate_accessible_svg` are
    /// advertised by the combined `kitty-tools` server — no longer its own
    /// process (`visualizations` is retired). On by default — like
    /// `wasm_math_mcp_enabled`, it's a safe, broadly useful, credential-free
    /// tool (accessible HTML tables/SVG diagrams rendered client-side in an
    /// iframe). Drives kitty-tools's `KITTY_VIZ_ENABLED` env var.
    #[serde(default = "default_true")]
    pub visualizations_enabled: bool,
    /// Whether the bundled `kitty-tools` server (see `plugins/kitty-tools/`)
    /// is registered+enabled as a BigTiny MCP server. This one Rust process
    /// hosts all 21 tools the base rewrite plan calls for: the 18 always-on
    /// `lean_*` tools (shell/workspace/5 file/3 word/4 cache/4 scratchpad —
    /// `replacement-mcp`'s entire surface), plus Brave search and the 2
    /// visualization tools, each of those two gated by their own flag above
    /// rather than this one. These 18 used to live in `replacement-mcp`,
    /// gated by `replacement_mcp_enabled`; on first load after the split,
    /// `migrate_kitty_split_enabled` carries that flag's value forward so a
    /// user who'd disabled `replacement_mcp_enabled` doesn't silently regain
    /// this tool set, and vice versa.
    #[serde(default = "default_true")]
    pub kitty_tools_enabled: bool,
    /// One-shot marker for the carry-forward migration above — same pattern
    /// as `replacement_mcp_default_migrated`.
    #[serde(default)]
    pub kitty_tools_default_migrated: bool,
    /// Whether the bundled `kitty-web` web-search/web-scrape server (see
    /// `plugins/kitty-web/`) is registered+enabled as a BigTiny MCP server.
    /// This Rust process is the replacement for the web half of the retired
    /// `kitty-docs-web` server (see `kitty_docs_web_enabled` below, kept only
    /// as the source value of the carry-forward migration).
    #[serde(default = "default_true")]
    pub kitty_web_enabled: bool,
    /// One-shot marker for the kitty-docs-web -> kitty-web carry-forward
    /// migration below — same pattern as `kitty_tools_default_migrated`.
    #[serde(default)]
    pub kitty_web_default_migrated: bool,
    /// Retired: `kitty-docs-web` no longer exists as its own process — its
    /// web tools moved to `kitty-web` (Rust) and its PDF/Excel tools moved
    /// to `kitty-tools` (Rust). Kept (not removed) purely as the source value
    /// `migrate_kitty_web_enabled` carries forward into `kitty_web_enabled` on
    /// an existing install's first load after the split — removing it would
    /// break that migration for anyone who hasn't upgraded past it yet.
    #[serde(default = "default_true")]
    pub kitty_docs_web_enabled: bool,
    #[serde(default)]
    pub kitty_docs_web_default_migrated: bool,
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
    /// convention as the other bundled plugins) if present, else `cargo`
    /// for dev convenience (paired with `bigtiny_args`'s `run
    /// --manifest-path plugins/bigtiny_rust/Cargo.toml` default, so
    /// `cargo tauri dev` still works from a source checkout).
    #[serde(default = "default_bigtiny_command")]
    pub bigtiny_command: String,
    /// Arguments before `--port`/`--host`. Empty for the bundled exe; a
    /// `cargo run` against `plugins/bigtiny_rust/` for the dev fallback.
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
    /// BigTiny background context-compaction settings, relayed to the daemon
    /// as `BIGTINY_SUMMARIZER__*` env vars at spawn
    /// (`lifecycle::bigtiny_proc::spawn`) — BigTiny only ever reads config via
    /// env/its own `--config` YAML, never anything Kitty writes directly, so
    /// this is the one place these settings need to exist on the Kitty side.
    #[serde(default)]
    pub summarizer: SummarizerSettings,
    /// BigTiny context-window/compaction budget settings, relayed as
    /// `BIGTINY_TOKEN_MANAGEMENT__*` env vars at spawn (same mechanism and
    /// rationale as `summarizer` above). `#[serde(default)]` covers loading
    /// a pre-existing config file that predates this field — no explicit
    /// migration function needed, unlike the value-changing `migrate_*`
    /// functions in `load` below.
    #[serde(default)]
    pub token_management: TokenManagementSettings,
    /// See `Config::memory`. Field names mirror BigTiny's own `MemoryConfig`
    /// (`plugins/bigtiny_rust/src/config.rs`) so the two don't drift apart.
    /// `#[serde(default)]` covers loading a pre-existing config file that
    /// predates this field — no explicit migration function needed, unlike
    /// the value-changing `migrate_*` functions in `load` below.
    #[serde(default)]
    pub memory: MemorySettings,
    /// The local llama.cpp engine's tunable knobs (docs/ANDROID.md §3.2, §6.1)
    /// — relayed as `BIGTINY_LOCAL__*` env vars at spawn, same mechanism as
    /// `summarizer`/`token_management`/`memory` above. Model *paths* are
    /// resolved separately in `bigtiny_proc::spawn` from `summarizer.model` /
    /// `adaptive_pathway_embedding_model` (GGUF ids, not paths) — this struct
    /// is everything else `LocalEngineConfig` accepts. `#[serde(default)]`
    /// covers a pre-existing config file; no migration needed, this is a new
    /// field with no prior value to carry forward.
    #[serde(default)]
    pub local: LocalModelSettings,
}

/// See `Config::local`. Field names and defaults mirror BigTiny's own
/// `LocalEngineConfig` (`plugins/bigtiny_rust/src/config.rs`) exactly, so the
/// two structs can't drift apart silently — a field renamed on one side and
/// not the other would otherwise fail only at runtime, as an env var the
/// daemon never reads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocalModelSettings {
    #[serde(default = "default_local_n_ctx")]
    pub n_ctx: u32,
    #[serde(default = "default_local_embed_n_ctx")]
    pub embed_n_ctx: u32,
    #[serde(default = "default_local_n_batch")]
    pub n_batch: u32,
    /// `0` = let llama.cpp pick from the host's core count.
    #[serde(default)]
    pub n_threads: i32,
    /// `-1` = all layers to the selected backend; `0` is CPU-only.
    #[serde(default = "default_local_n_gpu_layers")]
    pub n_gpu_layers: i32,
    /// `"auto"` (default) | `"cuda"` | `"vulkan"` | `"cpu"` — see
    /// `bigtiny_rust::local::backend`. Only `"cpu"` and `"auto"` do anything
    /// on current builds: no GPU cargo feature is enabled yet, so the device
    /// registry reports CPU only and the other two fall back to it.
    #[serde(default = "default_local_backend")]
    pub backend: String,
    /// `"last"` | `"mean"` | `"cls"` — belongs with the embed model pin, not
    /// the engine, but lives here rather than a fourth place since Kitty has
    /// nowhere else that's specifically "embedding settings."
    #[serde(default = "default_local_embed_pooling")]
    pub embed_pooling: String,
    /// `"f16"` (default, always safe) | `"q8_0"` | `"q4_0"` | `"q4_1"` |
    /// `"q5_0"` | `"q5_1"`. An advanced knob — see
    /// `bigtiny_rust::local::engine::parse_kv_cache_type`'s doc comment for
    /// why a non-default value's safety on a given backend isn't guaranteed.
    #[serde(default = "default_local_cache_type")]
    pub cache_type_k: String,
    #[serde(default = "default_local_cache_type")]
    pub cache_type_v: String,
}

fn default_local_n_ctx() -> u32 {
    4096
}
fn default_local_embed_n_ctx() -> u32 {
    512
}
fn default_local_n_batch() -> u32 {
    512
}
fn default_local_n_gpu_layers() -> i32 {
    -1
}
fn default_local_embed_pooling() -> String {
    "last".to_string()
}
fn default_local_backend() -> String {
    "auto".to_string()
}
fn default_local_cache_type() -> String {
    "f16".to_string()
}

impl Default for LocalModelSettings {
    fn default() -> Self {
        Self {
            n_ctx: default_local_n_ctx(),
            embed_n_ctx: default_local_embed_n_ctx(),
            n_batch: default_local_n_batch(),
            n_threads: 0,
            n_gpu_layers: default_local_n_gpu_layers(),
            backend: default_local_backend(),
            embed_pooling: default_local_embed_pooling(),
            cache_type_k: default_local_cache_type(),
            cache_type_v: default_local_cache_type(),
        }
    }
}

/// See `Config::summarizer`. `enabled` mirrors BigTiny's own
/// `SummarizerConfig.enabled` (relayed via `BIGTINY_SUMMARIZER__ENABLED`),
/// but `model` is Kitty-side only, and does something different than it used
/// to: it's the GGUF id `lifecycle::bigtiny_proc::spawn` resolves into
/// `BIGTINY_LOCAL__MODEL_PATH` (docs/ANDROID.md §4.1) — the daemon's own
/// `SummarizerConfig` no longer has a `model` field at all, since the local
/// summarizer gets its model from `[local]`, not `[summarizer]`.
///
/// `keep_alive` is gone. It was an Ollama-native `keep_alive` value
/// ("0"/"5m"/"-1") for the now-deleted Ollama-only `SummarizerClient`; the
/// in-process engine's residency is the slot manager's job, and nothing here
/// maps to it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SummarizerSettings {
    pub enabled: bool,
    pub model: String,
}

impl Default for SummarizerSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            model: DEFAULT_SUMMARIZER_GGUF.to_string(),
        }
    }
}

/// See `Config::token_management`. Field names/defaults mirror BigTiny's own
/// `TokenManagementConfig` (`plugins/bigtiny/bigtiny/config.py`) so the two
/// don't drift apart, but this is Kitty's independent copy — BigTiny's
/// Python defaults still apply if the daemon is ever launched without these
/// env vars set at all (e.g. a source checkout run directly, bypassing
/// Kitty).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TokenManagementSettings {
    pub max_context_tokens: u32,
    pub max_live_tail_tokens: u32,
    pub message_mask_head_lines: u32,
    pub message_mask_tail_lines: u32,
}

impl Default for TokenManagementSettings {
    fn default() -> Self {
        Self {
            max_context_tokens: 64000,
            max_live_tail_tokens: 24000,
            message_mask_head_lines: 10,
            message_mask_tail_lines: 10,
        }
    }
}

/// See `Config::memory`. Field names/defaults mirror BigTiny's own
/// `MemoryConfig` (`plugins/bigtiny_rust/src/config.rs`) so the two don't
/// drift apart — this is Kitty's independent copy; BigTiny's own defaults
/// still apply if the daemon is ever launched without these env vars.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct MemorySettings {
    /// Minimum FTS5 bm25 relevance score for pre-flight memory recall to
    /// inject context (higher = fewer, more relevant hits). `None` disables
    /// the threshold gate (inject whenever intent matches).
    pub bm25_threshold: Option<f64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkeys: vec!["Alt+Space".to_string()],
            clipboard_hotkey: Some("Ctrl+Alt+Space".to_string()),
            open_window_hotkey: None,
            default_context_folder: None,
            setup_completed: false,
            theme: "light".to_string(),
            notifications: NotificationPrefs::default(),
            remember_overlay_position: true,
            providers: Vec::new(),
            active_provider_id: None,
            strict_remote_mode: false,
            folders: Vec::new(),
            session_folders: HashMap::new(),
            show_artifacts: true,
            session_modes: HashMap::new(),
            session_efforts: HashMap::new(),
            adaptive_pathway_enabled: default_adaptive_pathway_enabled(),
            adaptive_pathway_embedding_model: default_ap_embedding_model(),
            replacement_mcp_enabled: default_true(),
            // A brand-new config needs no flip, so it starts already-migrated.
            replacement_mcp_default_migrated: true,
            wasm_math_mcp_enabled: default_true(),
            kitty_wasm_enabled: default_true(),
            // A brand-new config needs no carry-forward, so it starts
            // already-migrated — same reasoning as
            // `replacement_mcp_default_migrated` above.
            kitty_wasm_default_migrated: true,
            brave_mcp_search_enabled: false,
            visualizations_enabled: default_true(),
            kitty_tools_enabled: default_true(),
            // A brand-new config needs no carry-forward, so it starts
            // already-migrated — same reasoning as
            // `replacement_mcp_default_migrated` above.
            kitty_tools_default_migrated: true,
            kitty_web_enabled: default_true(),
            kitty_web_default_migrated: true,
            kitty_docs_web_enabled: default_true(),
            kitty_docs_web_default_migrated: true,
            scheduled_tasks: Vec::new(),
            ollama_enabled: default_true(),
            bigtiny_command: default_bigtiny_command(),
            bigtiny_args: default_bigtiny_args(),
            bigtiny_dir: None,
            recipes: recipes::builtin_templates(),
            summarizer: SummarizerSettings::default(),
            token_management: TokenManagementSettings::default(),
            memory: MemorySettings::default(),
            local: LocalModelSettings::default(),
        }
    }
}

fn default_bigtiny_command() -> String {
    bundled_plugin_path("bigtiny-daemon.exe").unwrap_or_else(|| "cargo".to_string())
}

/// Empty when the bundled exe was found — it needs no extra args. Otherwise,
/// the dev-convenience fallback runs the daemon straight out of the
/// `plugins/bigtiny_rust` source checkout via `cargo run` (this backend is
/// pure Rust now — no Python interpreter/package involved at all), matching
/// the old `python -m bigtiny` fallback's purpose: `cargo tauri dev` should
/// work without requiring `plugins/build.py` to have run first.
/// `--manifest-path` is resolved from this crate's own compile-time location
/// (`CARGO_MANIFEST_DIR`) rather than a path relative to the process's
/// working directory, so it's correct regardless of where `cargo tauri dev`
/// happens to be invoked from.
fn default_bigtiny_args() -> Vec<String> {
    if bundled_plugin_path("bigtiny-daemon.exe").is_some() {
        Vec::new()
    } else {
        let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("plugins")
            .join("bigtiny_rust")
            .join("Cargo.toml");
        vec![
            "run".to_string(),
            "--quiet".to_string(),
            "--manifest-path".to_string(),
            manifest_path.to_string_lossy().into_owned(),
            "--bin".to_string(),
            "bigtiny-daemon".to_string(),
            "--".to_string(),
        ]
    }
}

fn default_true() -> bool {
    true
}

fn default_adaptive_pathway_enabled() -> bool {
    true
}

/// Resolves `<name>` next to the currently-running executable, if it exists.
/// Shared by every bundled-plugin default (e.g. `kitty-tools`/`kitty-web`
/// exe paths for BigTiny's MCP server registration — see
/// `bigtiny::mcp::ensure_builtin_servers`).
pub(crate) fn bundled_plugin_path(name: &str) -> Option<String> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let candidate = dir.join(name);
    candidate
        .exists()
        .then(|| candidate.to_string_lossy().into_owned())
}

fn default_ap_embedding_model() -> String {
    DEFAULT_EMBEDDING_GGUF.to_string()
}

/// GGUF ids (file stems) of the two models the local engine uses by default,
/// matching docs/ANDROID.md §9. Ids rather than Ollama tags since Phase 2b:
/// these name a file in `models_dir()`, resolved by `crate::models::resolve`.
// LiteRT migration: these now name LiteRT artifacts (`.tflite` embedder,
// `.litertlm` generative summarizer), not GGUFs. Names kept for churn reasons;
// the values match `src/lib/curated_models.ts` and what `bigtiny_env` resolves.
pub const DEFAULT_EMBEDDING_GGUF: &str = "embeddinggemma-300M_seq256_mixed-precision.tflite";
pub const DEFAULT_SUMMARIZER_GGUF: &str = "gemma-4-E2B-it.litertlm";

/// Ollama tags these two settings held before Phase 2b, carried forward by
/// `migrate_model_tags_to_gguf`.
const LEGACY_EMBEDDING_TAG: &str = "qwen3-embedding:0.6b";
/// The q4_k_m id this briefly pointed at. **Qwen never published a q4 for
/// this model** — the official repo has only `Q8_0` and `f16` — so any config
/// carrying this name references a file that cannot be downloaded, and every
/// embedding load would fail with "model not found" forever. Migrated, not
/// merely re-defaulted: `adaptive_pathway_embedding_model` is a persisted
/// field, so an existing install keeps the dead name unless something
/// rewrites it.
const UNAVAILABLE_EMBEDDING_GGUF: &str = "Qwen3-Embedding-0.6B-q4_k_m";
const LEGACY_SUMMARIZER_TAGS: [&str; 2] = ["LFM2.5-1.2b", "qwen3.5:0.8b"];

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
/// Where the app may write, when the platform can't be asked with `dirs`.
///
/// **Android has no XDG or Known-Folder equivalent**, so `dirs::config_dir()`
/// and `dirs::data_local_dir()` both return `None` there and every path
/// derived from them fails. Observed on a Pixel 10 as two cascading errors:
/// `config load failed (could not resolve the app config directory)`, then
/// the daemon falling all the way back to a relative `./.bigtiny` and dying
/// on `Read-only file system` — because the process's cwd on Android is `/`.
///
/// The real answer comes from the Android `Context`
/// (`/data/user/0/<package>/files`), which only Tauri's `PathResolver` can
/// produce. It is stashed here at startup so the rest of this module keeps
/// its `AppHandle`-free signatures — threading a handle through every path
/// helper would touch far more code than the one platform needs.
static APP_DIRS: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Install the platform-resolved app directory. Call once, as early in
/// startup as an `AppHandle` exists; ignored if already set.
///
/// Android-only, because it is the only platform that needs it: `dirs`
/// answers everywhere else, so on desktop `APP_DIRS` stays unset and every
/// path below resolves exactly as it did before. Gated rather than
/// `allow(dead_code)`d so a future desktop caller is a compile error to think
/// about, not a silent override of the platform's own conventions.
#[cfg(target_os = "android")]
pub fn init_app_dir(dir: PathBuf) {
    let _ = APP_DIRS.set(dir);
}

/// The base directory app data lives under. Prefers whatever
/// [`init_app_dir`] was given, then the platform's own config dir.
fn app_base_dir() -> Result<PathBuf, ConfigError> {
    if let Some(dir) = APP_DIRS.get() {
        return Ok(dir.clone());
    }
    dirs::config_dir().ok_or(ConfigError::NoConfigDir)
}

pub(crate) fn config_dir() -> Result<PathBuf, ConfigError> {
    let base = app_base_dir()?;
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

/// `%LOCALAPPDATA%/Kitty/models/` (created if missing) — downloaded GGUFs.
///
/// **The only path here that isn't under `%APPDATA%`**, deliberately: a
/// roaming profile syncs, and these files are hundreds of megabytes to
/// several gigabytes each. `dirs::data_local_dir()` is the non-roaming
/// equivalent, and matches where `tools/local_engine_lab.py` already looks.
pub fn models_dir() -> Result<PathBuf, ConfigError> {
    // On Android there is no roaming/local split to respect — app storage is
    // app storage — so this falls through to the same base as everything
    // else rather than failing on a `dirs` call that answers `None` there.
    let base = match APP_DIRS.get() {
        Some(dir) => dir.clone(),
        None => dirs::data_local_dir().ok_or(ConfigError::NoConfigDir)?,
    };
    let dir = base.join("Kitty").join("models");
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
            Ok(migrate_theme_default_to_light(migrate_model_tags_to_gguf(
                migrate_kitty_wasm_enabled(migrate_kitty_web_enabled(
                    migrate_kitty_split_enabled(migrate_replacement_mcp_enabled(
                        migrate_bigtiny_launch_command(migrate_recipes(migrate_hotkeys(
                            config, &text,
                        ))),
                    )),
                )),
            )))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e.into()),
    }
}

/// Load config, and on a corrupt-file failure back the bad file up (before
/// falling back to defaults) so a later save never silently overwrites the
/// user's only copy. Returns the config plus `Some(backup path)` when a
/// recovery actually happened — lib.rs stashes that in `AppState` so the
/// frontend can show a one-time notice.
pub fn load_with_recovery() -> (Config, Option<PathBuf>) {
    match load() {
        Ok(cfg) => (cfg, None),
        // Not a recovery case, and saying so matters. On Android this is the
        // *expected* result of the first call: `lib.rs` loads once before
        // Tauri exists, and `APP_DIRS` can only be filled in from `setup`
        // (see `init_app_dir`), which reloads. Routing that through the
        // corrupt-file path logged a WARN about backing up a config that was
        // never read, on every single launch — noise that would camouflage
        // the real thing. It is also wrong everywhere else: if the directory
        // cannot be resolved there is no file, so there is nothing to be
        // corrupt and nothing to back up.
        Err(ConfigError::NoConfigDir) => {
            tracing::debug!("config directory not resolvable yet; using defaults for now");
            (Config::default(), None)
        }
        Err(e) => {
            let backup = backup_corrupt_config();
            tracing::warn!(
                "config load failed ({e}); backed up to {}",
                backup
                    .as_ref()
                    .map(|b| b.display().to_string())
                    .unwrap_or_else(|| "config.json (nothing to back up)".to_string())
            );
            (Config::default(), backup)
        }
    }
}

/// Best-effort backup of the current `config.json` (which failed to parse)
/// to `config.json.corrupt-<unix_timestamp>` next to it — a same-directory
/// rename when possible, else a copy. Never a hard failure: returns `None`
/// when there's nothing to back up or the backup fails, and callers just
/// fall back to defaults either way.
pub fn backup_corrupt_config() -> Option<PathBuf> {
    let path = config_path().ok()?;
    backup_corrupt_config_file(&path)
}

/// Path-explicit half of [`backup_corrupt_config`], so the logic is
/// unit-testable against temp-dir paths instead of the machine's real
/// `%APPDATA%/Kitty/config.json`.
pub(crate) fn backup_corrupt_config_file(path: &Path) -> Option<PathBuf> {
    if !path.exists() {
        return None;
    }
    let timestamp = chrono::Utc::now().timestamp();
    let backup = path.with_file_name(format!("config.json.corrupt-{timestamp}"));
    if fs::rename(path, &backup).is_ok() {
        Some(backup)
    } else {
        fs::copy(path, &backup).ok().map(|_| backup)
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

/// Self-heals an existing install's `bigtiny_command`/`bigtiny_args` off
/// either dev-convenience default — the original Python-era `python -m
/// bigtiny`, or the current `cargo run --manifest-path .../bigtiny_rust
/// ...` (see `default_bigtiny_args`) — onto the bundled exe, once one is
/// present, including a stale-absolute-path self-heal
/// (`command_path_is_stale`). A deliberate override (e.g. `uv run bigtiny`,
/// or a source checkout via `bigtiny_dir`) is left untouched.
fn migrate_bigtiny_launch_command(mut config: Config) -> Config {
    const OLD_PYTHON_COMMAND: &str = "python";
    let old_python_args = ["-m".to_string(), "bigtiny".to_string()];
    let is_cargo_dev_fallback = config.bigtiny_command == "cargo"
        && config.bigtiny_args.iter().any(|a| a == "bigtiny-daemon");
    let is_resolved_path =
        config.bigtiny_command.contains('/') || config.bigtiny_command.contains('\\');
    let bundled = bundled_plugin_path("bigtiny-daemon.exe");
    // Tauri's `externalBin` build step stages a copy of every sidecar next
    // to `current_exe()` on *every* build, dev included (`cargo tauri dev`
    // is not the unbundled case it looks like — `target/debug/` ends up with
    // its own `bigtiny-daemon.exe` alongside `kitty.exe`, copied fresh each
    // time `src-tauri/binaries/...` changes). So a bundled sibling is nearly
    // always `Some` in practice, and it is always the *authoritative*
    // location once present. A resolved path that still points somewhere
    // else — e.g. a real install's `%LOCALAPPDATA%\Kitty\bigtiny-daemon.exe`,
    // left over in this same user's config from before a `cargo tauri dev`
    // checkout ever existed — is exactly the case `command_path_is_stale`
    // can't catch: that file is perfectly real and loadable, so nothing
    // complains, it just silently keeps running whatever daemon was frozen
    // into it (a different backend/version) instead of the one that was
    // just rebuilt next door.
    let points_elsewhere_than_bundled = is_resolved_path
        && bundled
            .as_deref()
            .is_some_and(|b| b != config.bigtiny_command);
    let stale = (config.bigtiny_command == OLD_PYTHON_COMMAND
        && config.bigtiny_args == old_python_args)
        || is_cargo_dev_fallback
        || command_path_is_stale(&config.bigtiny_command)
        || points_elsewhere_than_bundled
        || (bundled.is_none() && is_resolved_path);
    if stale {
        if let Some(bundled) = bundled {
            config.bigtiny_command = bundled;
            config.bigtiny_args = Vec::new();
        } else if is_resolved_path {
            config.bigtiny_command = default_bigtiny_command();
            config.bigtiny_args = default_bigtiny_args();
        }
    }
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

/// One-time carry-forward for the `kitty-tools`/`kitty-docs-web` split: the
/// Word tools and the PDF/Excel/web/search tools both used to live in
/// `replacement-mcp`, gated by the single `replacement_mcp_enabled` flag.
/// Splitting them into their own servers with their own toggles must not
/// silently change what's enabled for an existing install — an install that
/// had `replacement_mcp_enabled: false` must land on both new flags `false`
/// too, and one that had it `true` must land on both `true`. Runs *after*
/// `migrate_replacement_mcp_enabled` so it reads that flag's already-settled
/// value, not a stale pre-migration one. Guarded the same one-shot way: a
/// user who later flips either new flag independently keeps that choice.
fn migrate_kitty_split_enabled(mut config: Config) -> Config {
    if !config.kitty_tools_default_migrated {
        config.kitty_tools_enabled = config.replacement_mcp_enabled;
        config.kitty_tools_default_migrated = true;
    }
    if !config.kitty_docs_web_default_migrated {
        config.kitty_docs_web_enabled = config.replacement_mcp_enabled;
        config.kitty_docs_web_default_migrated = true;
    }
    config
}

/// One-time carry-forward for the `kitty-docs-web` -> `kitty-web` split: the
/// retired `kitty-docs-web` server's `kitty_docs_web_enabled` flag is carried
/// into `kitty_web_enabled` so an existing install that had web tools
/// disabled doesn't suddenly gain them. Runs after `migrate_kitty_split_enabled`
/// so it reads that flag's already-settled value. Guarded the same one-shot
/// way: a user who later flips `kitty_web_enabled` independently keeps that
/// choice.
fn migrate_kitty_web_enabled(mut config: Config) -> Config {
    if !config.kitty_web_default_migrated {
        config.kitty_web_enabled = config.kitty_docs_web_enabled;
        config.kitty_web_default_migrated = true;
    }
    config
}

/// One-time carry-forward for the `wasm-math-mcp` -> `kitty-wasm` cutover:
/// the retired `wasm-math-mcp` server's `wasm_math_mcp_enabled` flag is
/// carried into `kitty_wasm_enabled` so an existing install that had math
/// execution disabled doesn't suddenly gain it. Guarded the same one-shot
/// way as `migrate_kitty_web_enabled`: a user who later flips
/// `kitty_wasm_enabled` independently keeps that choice.
/// Phase 2b: `summarizer.model` and `adaptive_pathway_embedding_model` used
/// to hold Ollama tags (`qwen3-embedding:0.6b`), and now hold GGUF ids naming
/// a file in `models_dir()`. A saved tag would resolve to nothing, silently
/// leaving both engine slots unconfigured — chat falling back to the active
/// provider and beliefs dropping to lexical hashing, with no error anywhere
/// to explain it. Rewrite the known tags to their GGUF equivalents.
///
/// Only the exact tags Kitty itself ever wrote are touched. Anything a user
/// typed by hand is left alone: it may well name a GGUF they downloaded
/// themselves, and guessing would be worse than leaving it.
fn migrate_model_tags_to_gguf(mut config: Config) -> Config {
    // LiteRT migration: GGUFs are gone. Any Ollama tag, the old GGUF defaults,
    // or any lingering `.gguf` id must move to the LiteRT defaults, or the slot
    // resolves to nothing and silently degrades. A `.gguf` value can no longer
    // name anything the engine can load, so the earlier "leave hand-typed values
    // alone" caveat no longer applies to GGUF ids.
    let emb = &config.adaptive_pathway_embedding_model;
    if emb == LEGACY_EMBEDDING_TAG
        || emb == UNAVAILABLE_EMBEDDING_GGUF
        || emb.ends_with(".gguf")
        || emb == "Qwen3-Embedding-0.6B-Q8_0"
    {
        config.adaptive_pathway_embedding_model = DEFAULT_EMBEDDING_GGUF.to_string();
    }
    let sum = config.summarizer.model.as_str();
    if LEGACY_SUMMARIZER_TAGS.contains(&sum)
        || sum.ends_with(".gguf")
        || sum == "LFM2.5-1.2B-Instruct-Q4_K_M"
    {
        config.summarizer.model = DEFAULT_SUMMARIZER_GGUF.to_string();
    }
    config
}

fn migrate_kitty_wasm_enabled(mut config: Config) -> Config {
    if !config.kitty_wasm_default_migrated {
        config.kitty_wasm_enabled = config.wasm_math_mcp_enabled;
        config.kitty_wasm_default_migrated = true;
    }
    config
}

/// release-fixes item 18: the built-in `"default"` theme was renamed
/// `"light"` for a clearer name next to `"dark"`. Normalize an existing
/// config.json's old id in memory rather than forcing an immediate write —
/// the value naturally becomes `"light"` on disk the next time anything
/// calls `save` (Settings → Appearance's own Save, or any other section's),
/// matching how `migrate_hotkeys` above handles its own one-time reshape.
fn migrate_theme_default_to_light(mut config: Config) -> Config {
    if config.theme == "default" {
        config.theme = "light".to_string();
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
        assert_eq!(
            fs::read_to_string(new.join("themes").join("x.css")).unwrap(),
            "x"
        );
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
    fn theme_default_migrates_to_light() {
        let raw = r#"{"theme":"default"}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let cfg = migrate_theme_default_to_light(cfg);
        assert_eq!(cfg.theme, "light");
    }

    #[test]
    fn theme_other_than_default_is_left_alone() {
        for theme in ["dark", "light", "my-custom-theme"] {
            let cfg = Config {
                theme: theme.to_string(),
                ..Config::default()
            };
            let cfg = migrate_theme_default_to_light(cfg);
            assert_eq!(cfg.theme, theme);
        }
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
        assert_eq!(back.default_context_folder, None);
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
        // A config predating the pathway engine's `adaptive_pathway_enabled`
        // field must still load, on by default (UX-simplification decision —
        // enabled by default for fresh/pre-existing-field configs alike).
        let back: Config = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert!(back.adaptive_pathway_enabled);
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
    fn migrate_bigtiny_launch_command_is_a_noop_for_cargo_dev_fallback_with_no_bundled_binary() {
        let cfg = Config {
            bigtiny_command: "cargo".to_string(),
            bigtiny_args: default_bigtiny_args(),
            ..Config::default()
        };
        let migrated = migrate_bigtiny_launch_command(cfg);
        assert_eq!(migrated.bigtiny_command, "cargo");
        assert!(migrated.bigtiny_args.iter().any(|a| a == "bigtiny-daemon"));
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
    fn migrate_bigtiny_launch_command_without_a_bundled_sibling_resets_a_stale_resolved_path_to_dev_default(
    ) {
        // `bundled_plugin_path` resolves next to the *test binary*, which has
        // no `bigtiny-daemon.exe` sibling — the realistic `cargo tauri dev`
        // case. A resolved filesystem path in `bigtiny_command` here can
        // only be a leftover from a previous real install (a bundled build
        // always writes one of these; hand-configuring dev mode uses a bare
        // PATH command instead) — even though the file itself still exists,
        // it should be reset to the dev-convenience default rather than left
        // in place, since that default's own `bundled_plugin_path` lookup
        // will *also* find nothing here and correctly fall through to the
        // `cargo run ...` fallback.
        let dir = std::env::temp_dir().join(format!("kitty-stale-test-{}", uuid_like()));
        fs::create_dir_all(&dir).unwrap();
        let stale_daemon = dir.join("bigtiny-daemon.exe");
        fs::write(&stale_daemon, b"a real, still-loadable previous build").unwrap();

        let cfg = Config {
            bigtiny_command: stale_daemon.to_str().unwrap().to_string(),
            bigtiny_args: vec!["--some-flag".to_string()],
            ..Config::default()
        };
        let migrated = migrate_bigtiny_launch_command(cfg);
        assert_eq!(migrated.bigtiny_command, "cargo");
        assert!(migrated.bigtiny_args.iter().any(|a| a == "bigtiny-daemon"));

        fs::remove_dir_all(&dir).unwrap();
    }

    fn uuid_like() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        format!(
            "{}-{:?}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            std::thread::current().id()
        )
    }

    /// A config predating `[local]` must still load, defaulting every knob to
    /// the value `LocalEngineConfig` on the daemon side itself defaults to —
    /// the whole point of the two structs mirroring each other is that
    /// "unset on the Kitty side" and "unset on the daemon side" mean the same
    /// thing, so the daemon never receives a value the user didn't choose.
    #[test]
    fn old_shape_config_defaults_local_settings() {
        let back: Config = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(back.local, LocalModelSettings::default());
        assert_eq!(back.local.n_ctx, 4096);
        assert_eq!(back.local.n_gpu_layers, -1);
        assert_eq!(back.local.cache_type_k, "f16");
        assert_eq!(back.local.cache_type_v, "f16");
    }

    #[test]
    fn old_shape_config_migrates_embedding_model_default() {
        // A config predating the embedding-model requirement must still load,
        // defaulting to the one pinned cross-compatible model every user
        // shares — a GGUF id since Phase 2b, not an Ollama tag.
        let back: Config = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(back.adaptive_pathway_embedding_model, DEFAULT_EMBEDDING_GGUF);
    }

    /// The embedding pin briefly named `Qwen3-Embedding-0.6B-q4_k_m`, which
    /// Qwen never published — the official repo has only `Q8_0` and `f16`,
    /// so that filename 404s on Hugging Face. Because the field is
    /// persisted, changing the default alone would leave every existing
    /// install pointed at a file it can never download, and semantic recall
    /// silently stuck on the hash-space fallback forever.
    #[test]
    fn a_config_pinned_to_the_nonexistent_q4_embedding_is_repointed() {
        let stale = Config {
            adaptive_pathway_embedding_model: UNAVAILABLE_EMBEDDING_GGUF.to_string(),
            ..Config::default()
        };
        let migrated = migrate_model_tags_to_gguf(stale);
        assert_eq!(
            migrated.adaptive_pathway_embedding_model,
            DEFAULT_EMBEDDING_GGUF
        );
    }

    /// A user who deliberately chose some other embedder must keep it — the
    /// migration repoints exactly the two dead names, not anything unfamiliar.
    #[test]
    fn a_deliberately_chosen_embedding_model_survives_the_migration() {
        let chosen = Config {
            adaptive_pathway_embedding_model: "my-own-embedder".to_string(),
            ..Config::default()
        };
        let migrated = migrate_model_tags_to_gguf(chosen);
        assert_eq!(migrated.adaptive_pathway_embedding_model, "my-own-embedder");
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
        let cfg: Config = serde_json::from_str(r#"{"replacement_mcp_enabled":false}"#).unwrap();
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
    fn kitty_split_carries_forward_a_disabled_replacement_mcp() {
        // Pre-split shape: replacement_mcp_enabled already migrated to
        // false (a deliberate opt-out), no kitty-tools/kitty-docs-web keys
        // at all. Both new flags must land on false too, not the container
        // `default_true`.
        let cfg: Config = serde_json::from_str(
            r#"{"replacement_mcp_enabled":false,"replacement_mcp_default_migrated":true}"#,
        )
        .unwrap();
        let migrated = migrate_kitty_split_enabled(migrate_replacement_mcp_enabled(cfg));
        assert!(!migrated.kitty_tools_enabled);
        assert!(!migrated.kitty_docs_web_enabled);
        assert!(migrated.kitty_tools_default_migrated);
        assert!(migrated.kitty_docs_web_default_migrated);
    }

    #[test]
    fn kitty_split_carries_forward_an_enabled_replacement_mcp() {
        let cfg: Config = serde_json::from_str(r#"{}"#).unwrap();
        let migrated = migrate_kitty_split_enabled(migrate_replacement_mcp_enabled(cfg));
        assert!(migrated.kitty_tools_enabled);
        assert!(migrated.kitty_docs_web_enabled);
    }

    #[test]
    fn kitty_split_respects_an_opt_out_made_after_its_own_migration() {
        let cfg: Config = serde_json::from_str(
            r#"{"kitty_tools_enabled":false,"kitty_tools_default_migrated":true,"kitty_docs_web_enabled":true,"kitty_docs_web_default_migrated":true}"#,
        )
        .unwrap();
        let migrated = migrate_kitty_split_enabled(migrate_replacement_mcp_enabled(cfg));
        assert!(!migrated.kitty_tools_enabled);
        assert!(migrated.kitty_docs_web_enabled);
    }

    #[test]
    fn kitty_split_is_on_for_a_brand_new_config() {
        let cfg = Config::default();
        assert!(cfg.kitty_tools_enabled);
        assert!(cfg.kitty_docs_web_enabled);
        assert!(cfg.kitty_tools_default_migrated);
        assert!(cfg.kitty_docs_web_default_migrated);
    }

    #[test]
    fn kitty_web_carries_forward_a_disabled_docs_web_flag() {
        // Pre-split shape: kitty-docs-web (which hosted web search) was opted
        // out. kitty_web_enabled must land on false too, not the container
        // `default_true`, so a user who disabled web tools doesn't suddenly
        // gain them.
        let cfg: Config = serde_json::from_str(
            r#"{"kitty_docs_web_enabled":false,"kitty_docs_web_default_migrated":true}"#,
        )
        .unwrap();
        let migrated = migrate_kitty_web_enabled(cfg);
        assert!(!migrated.kitty_web_enabled);
        assert!(migrated.kitty_web_default_migrated);
    }

    #[test]
    fn kitty_web_is_on_for_a_brand_new_config() {
        let cfg = Config::default();
        assert!(cfg.kitty_web_enabled);
        assert!(cfg.kitty_web_default_migrated);
    }

    #[test]
    fn kitty_wasm_carries_forward_a_disabled_wasm_math_flag() {
        // Pre-cutover shape: wasm-math-mcp (which hosted math Python) was
        // opted out. kitty_wasm_enabled must land on false too, not the
        // container `default_true`, so a user who disabled math execution
        // doesn't suddenly gain it.
        let cfg: Config = serde_json::from_str(r#"{"wasm_math_mcp_enabled":false}"#).unwrap();
        let migrated = migrate_kitty_wasm_enabled(cfg);
        assert!(!migrated.kitty_wasm_enabled);
        assert!(migrated.kitty_wasm_default_migrated);
    }

    #[test]
    fn kitty_wasm_is_on_for_a_brand_new_config() {
        let cfg = Config::default();
        assert!(cfg.kitty_wasm_enabled);
        assert!(cfg.kitty_wasm_default_migrated);
    }

    #[test]
    fn kitty_wasm_keeps_an_independent_later_choice() {
        let cfg: Config = serde_json::from_str(
            r#"{"wasm_math_mcp_enabled":false,"kitty_wasm_default_migrated":true}"#,
        )
        .unwrap();
        let migrated = migrate_kitty_wasm_enabled(cfg);
        // Already migrated: a user who flipped kitty_wasm_enabled on
        // independently keeps it, regardless of the retired old flag.
        assert!(migrated.kitty_wasm_enabled);
    }

    #[test]
    fn old_shape_config_migrates_wizard_redesign_defaults() {
        // A config predating the wizard redesign (ollama_enabled) must still
        // load, with Ollama left enabled (pre-existing installs already have
        // it configured).
        let back: Config = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert!(back.ollama_enabled);
    }

    #[test]
    fn backup_corrupt_config_file_moves_the_bad_file_next_to_a_timed_backup() {
        let dir = temp_dir("corrupt");
        fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.json");
        fs::write(&config, b"{ this is not valid json !!!").unwrap();

        let backup = backup_corrupt_config_file(&config).expect("backup should succeed");

        // The original is gone (renamed — the whole point is that a later
        // `save` writes a fresh file, not the corrupt one) and the backup
        // holds the exact original bytes under a `config.json.corrupt-` name.
        assert!(!config.exists());
        assert!(
            backup
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("config.json.corrupt-"))
        );
        assert_eq!(fs::read(&backup).unwrap(), b"{ this is not valid json !!!");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn backup_corrupt_config_file_is_a_no_op_when_the_file_is_missing() {
        let dir = temp_dir("corrupt-missing");
        fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.json");
        assert_eq!(backup_corrupt_config_file(&config), None);
        fs::remove_dir_all(&dir).unwrap();
    }
}
