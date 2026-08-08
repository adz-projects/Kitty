//! CLI entry point for the BigTiny daemon. Parses the same `--host`/`--port`/
//! `--config`/`--secret` flags `plugins/bigtiny/bigtiny/__main__.py` does,
//! and honors the same `BIGTINY_*` environment variables Kitty's spawn code
//! (`src-tauri/src/lifecycle/bigtiny_proc.rs::spawn`) actually sets — Kitty
//! never passes `--secret`/`--config`, it relies entirely on
//! `BIGTINY_SECRET`, `BIGTINY_DATA_DIR`, and the `BIGTINY_SUMMARIZER__*`/
//! `BIGTINY_TOKEN_MANAGEMENT__*`/`BIGTINY_MEMORY__*` env vars (Python's
//! pydantic-settings `env_prefix`/`env_nested_delimiter` mechanism), so those
//! specific variables are the ones honored here — this is not a general
//! nested-env config loader.

use std::path::{Path, PathBuf};

use bigtiny_rust::config::BigTinyConfig;
use bigtiny_rust::RunOptions;

struct Args {
    host: String,
    port: u16,
    config_path: Option<String>,
    secret: Option<String>,
}

fn parse_args() -> Args {
    let argv: Vec<String> = std::env::args().collect();
    let mut host = "127.0.0.1".to_string();
    let mut port: u16 = 8080;
    let mut config_path: Option<String> = None;
    let mut secret: Option<String> = std::env::var("BIGTINY_SECRET").ok();

    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "--host" => {
                i += 1;
                if let Some(v) = argv.get(i) {
                    host = v.clone();
                }
            }
            "--port" => {
                i += 1;
                if let Some(v) = argv.get(i) {
                    if let Ok(p) = v.parse() {
                        port = p;
                    }
                }
            }
            "--config" => {
                i += 1;
                if let Some(v) = argv.get(i) {
                    config_path = Some(v.clone());
                }
            }
            "--secret" => {
                i += 1;
                if let Some(v) = argv.get(i) {
                    secret = Some(v.clone());
                }
            }
            // No hot-reload equivalent (uvicorn's --reload has nothing to do
            // here) — accepted and ignored so Kitty's --reload-less flag set
            // still parses if it's ever added.
            "--reload" => {}
            _ => {}
        }
        i += 1;
    }

    Args {
        host,
        port,
        config_path,
        secret,
    }
}

/// `BIGTINY_DATA_DIR` env var, or `~/.bigtiny` — matches
/// `plugins/bigtiny/bigtiny/paths.py::data_dir()` exactly, since Kitty's
/// `bigtiny_proc.rs::spawn` points this at `%APPDATA%/Kitty/bigtiny/`.
fn resolve_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("BIGTINY_DATA_DIR") {
        return PathBuf::from(dir);
    }
    dirs_home().join(".bigtiny")
}

fn dirs_home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Expand a leading `~/` in a config-supplied path against the resolved
/// home directory, matching Python's `Path(...).expanduser()`.
fn shellexpand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => dirs_home().join(rest),
        None => PathBuf::from(path),
    }
}

/// Applies the specific `BIGTINY_SUMMARIZER__*`/`BIGTINY_TOKEN_MANAGEMENT__*`/
/// `BIGTINY_MEMORY__*` env vars Kitty's spawn code sets — see this file's
/// module doc for why only these, not a general nested-env-var deserializer.
fn apply_env_overrides(config: &mut BigTinyConfig) {
    if let Ok(v) = std::env::var("BIGTINY_SUMMARIZER__ENABLED") {
        config.summarizer.enabled = v.eq_ignore_ascii_case("true") || v == "1";
    }
    if let Ok(v) = std::env::var("BIGTINY_SUMMARIZER__MODEL") {
        config.summarizer.model = v;
    }
    if let Ok(v) = std::env::var("BIGTINY_SUMMARIZER__KEEP_ALIVE") {
        config.summarizer.keep_alive = v;
    }
    if let Some(n) = std::env::var("BIGTINY_TOKEN_MANAGEMENT__MAX_CONTEXT_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        config.token_management.max_context_tokens = n;
    }
    if let Some(n) = std::env::var("BIGTINY_TOKEN_MANAGEMENT__MAX_LIVE_TAIL_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        config.token_management.max_live_tail_tokens = n;
    }
    if let Some(n) = std::env::var("BIGTINY_TOKEN_MANAGEMENT__MESSAGE_MASK_HEAD_LINES")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        config.token_management.message_mask_head_lines = n;
    }
    if let Some(n) = std::env::var("BIGTINY_TOKEN_MANAGEMENT__MESSAGE_MASK_TAIL_LINES")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        config.token_management.message_mask_tail_lines = n;
    }
    if let Ok(v) = std::env::var("BIGTINY_MEMORY__PREFLIGHT_ENABLED") {
        config.memory.preflight_enabled = v.eq_ignore_ascii_case("true") || v == "1";
    }
    if let Some(n) = std::env::var("BIGTINY_MEMORY__BM25_THRESHOLD")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        config.memory.bm25_threshold = Some(n);
    }
    if let Some(n) = std::env::var("BIGTINY_MEMORY__PREFLIGHT_RESULTS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        config.memory.preflight_results = n;
    }
    if let Some(n) = std::env::var("BIGTINY_MEMORY__ARTIFACTS_MAX_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        config.memory.artifacts_max_tokens = n;
    }
    // `PathwayConfig::enabled` defaults to `false` and, unlike every other
    // config section above, previously had NO env override at all. Since
    // Kitty (like every host) never passes a `--config` YAML, that made the
    // in-process behavioral-memory engine permanently dead in every real
    // deployment regardless of anything the host does -- this is the actual
    // toggle a host needs to opt in, mirroring `BIGTINY_SUMMARIZER__*`.
    if let Ok(v) = std::env::var("BIGTINY_PATHWAY__ENABLED") {
        config.pathway.enabled = v.eq_ignore_ascii_case("true") || v == "1";
    }
    if let Some(n) = std::env::var("BIGTINY_PATHWAY__LEARN_EVERY_N")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        config.pathway.learn_every_n = n;
    }
}

#[tokio::main]
async fn main() {
    let args = parse_args();

    let data_dir = resolve_data_dir();
    let db_path = data_dir.join("bigtiny.db").to_string_lossy().into_owned();

    let mut config = match &args.config_path {
        Some(p) => BigTinyConfig::load(Path::new(p)).unwrap_or_else(|e| {
            eprintln!("Failed to load config {p}: {e}");
            BigTinyConfig::default()
        }),
        None => BigTinyConfig::default(),
    };
    apply_env_overrides(&mut config);

    // `config.recipes.directory` was previously always ignored in favor of
    // a hardcoded `data_dir/recipes` — that's still the right zero-config
    // default (keeps recipes consolidated under `BIGTINY_DATA_DIR` /
    // Kitty's data root with no extra knob to manage), but an explicit
    // override via `--config` should actually take effect instead of being
    // silently dropped.
    let recipes_dir =
        if config.recipes.directory == bigtiny_rust::config::default_recipes_directory() {
            data_dir.join("recipes")
        } else {
            shellexpand_home(&config.recipes.directory)
        };

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(config.logging.level.clone()));
    // `json_format` was previously ignored too — only `level` (via
    // `EnvFilter`) ever actually applied.
    if config.logging.json_format {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }

    let options = RunOptions {
        host: args.host,
        port: args.port,
        db_path,
        secret: args.secret,
        // Preserves the historical desktop behavior of running
        // unauthenticated when no secret is configured — this CLI entry
        // point is the single-user-localhost case `AuthConfig::required`
        // exists to distinguish from. An embedding host on a platform where
        // loopback isn't process-private should set this `true` instead.
        require_secret: false,
        recipes_dir,
        data_dir: data_dir.to_string_lossy().into_owned(),
        // Env-only, no `--encryption-key` flag — matches `BIGTINY_SECRET`'s
        // own env-only convention (Kitty never passes secrets via argv).
        encryption_key: std::env::var("BIGTINY_ENCRYPTION_KEY").ok(),
        // No embedding host is involved for the CLI entry point: nothing
        // needs the bound port reported back (the `--port` flag already
        // fixed it), and only a process signal should stop this process.
        ready_tx: None,
        shutdown: None,
    };

    if let Err(e) = bigtiny_rust::run(config, options).await {
        eprintln!("BigTiny daemon exited with error: {e}");
        std::process::exit(1);
    }
}
