//! CLI entry point for the BigTiny daemon. Parses the same `--host`/`--port`/
//! `--config`/`--secret` flags `plugins/bigtiny/bigtiny/__main__.py` does.
//!
//! The `BIGTINY_*` environment contract this honors lives in
//! [`bigtiny_rust::env_contract`], not here — it has a second caller, the
//! embedded host Kitty uses on Android where there is no separate executable
//! to spawn (D8, §2.3). This file is now only argument parsing and the
//! process-lifetime concerns (tokio runtime, ctrl-c) that a library caller
//! supplies for itself.

use std::path::Path;

use bigtiny_rust::config::BigTinyConfig;
use bigtiny_rust::env_contract::{apply_env_overrides, resolve_data_dir, shellexpand_home};
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
                match argv.get(i) {
                    // An unparsable port used to be silently ignored, binding
                    // 8080 while the user (or a supervisor script) believes
                    // the daemon is elsewhere — "Kitty can't reach backend"
                    // with no hint why. Fail loudly at startup instead.
                    Some(v) => match v.parse() {
                        Ok(p) => port = p,
                        Err(_) => {
                            eprintln!("Invalid --port value {v:?}: expected an integer 0-65535");
                            std::process::exit(2);
                        }
                    },
                    None => {
                        eprintln!("--port requires a value");
                        std::process::exit(2);
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
