//! CLI entry point for the BigTiny daemon. Parses the same `--host`/`--port`/
//! `--config`/`--secret` flags `plugins/bigtiny/bigtiny/__main__.py` does.
//!
//! The `BIGTINY_*` environment contract this honors lives in
//! [`bigtiny2::env_contract`], not here — it has a second caller, the
//! embedded host Kitty uses on Android where there is no separate executable
//! to spawn (D8, §2.3). This file is now only argument parsing and the
//! process-lifetime concerns (tokio runtime, ctrl-c) that a library caller
//! supplies for itself.

use std::path::Path;

use bigtiny2::config::BigTinyConfig;
use bigtiny2::env_contract::{apply_env_overrides, resolve_data_dir, shellexpand_home};
use bigtiny2::RunOptions;

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


/// Worker threads for the async runtime.
///
/// A bare `#[tokio::main]` spawns one worker per CPU and allows up to 512
/// blocking threads, sized for a server saturating a machine. This is a
/// single-user desktop/phone daemon whose async work is overwhelmingly waiting
/// on a network socket or SQLite, so the extra workers buy nothing and each
/// carries a stack. Capped rather than fixed so a 4-core machine still gets 4.
///
/// Blocking threads are what `spawn_blocking` uses — here that is the LiteRT
/// embed/summarize actors plus token counting, a handful at a time, never
/// hundreds.
const MAX_WORKER_THREADS: usize = 4;
const MAX_BLOCKING_THREADS: usize = 16;
/// 2 MiB is the Rust default; the daemon has no deep-recursion paths, and on
/// Android this is multiplied across every worker in a memory-tight process.
const WORKER_STACK_SIZE: usize = 1024 * 1024;

fn main() {
    let worker_threads = std::thread::available_parallelism()
        .map(|n| n.get().min(MAX_WORKER_THREADS))
        .unwrap_or(2)
        .max(2);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .max_blocking_threads(MAX_BLOCKING_THREADS)
        .thread_stack_size(WORKER_STACK_SIZE)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
        .block_on(async_main());
}

async fn async_main() {
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
        if config.recipes.directory == bigtiny2::config::default_recipes_directory() {
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
        // V2 owns its own at-rest key. In V1 this came from *Kitty's*
        // Credential Manager, injected on every launch -- which inverts
        // ownership the moment any app can be the one to spawn the daemon.
        // `crypto::init` falls back to a self-managed key file in the data
        // dir, so the env var is now only an escape hatch for a host that
        // genuinely wants to supply one, and is V2-scoped so a stale
        // `BIGTINY_ENCRYPTION_KEY` from V1 cannot reach us.
        encryption_key: std::env::var("BIGTINYV2_ENCRYPTION_KEY").ok(),
        // No embedding host is involved for the CLI entry point: nothing
        // needs the bound port reported back (the `--port` flag already
        // fixed it), and only a process signal should stop this process.
        ready_tx: None,
        shutdown: None,
        idle_exit_mins: idle_exit_mins(),
    };

    if let Err(e) = bigtiny2::run(config, options).await {
        eprintln!("BigTiny daemon exited with error: {e}");
        std::process::exit(1);
    }
}

/// Minutes of inactivity before the daemon exits on its own, or `None` to stay
/// up indefinitely (`--no-idle-exit`, or `BIGTINYV2_IDLE_EXIT_MINS=0`).
///
/// Defaults to 30. This is what replaces V1's "whoever spawned me kills me on
/// exit" lifetime: with several clients attached, no single one of them may
/// decide the daemon is finished, so the daemon decides for itself. A
/// service-style deployment that wants it always-on passes `--no-idle-exit`.
fn idle_exit_mins() -> Option<u64> {
    const DEFAULT_IDLE_EXIT_MINS: u64 = 30;

    if std::env::args().any(|a| a == "--no-idle-exit") {
        return None;
    }
    match std::env::var("BIGTINYV2_IDLE_EXIT_MINS") {
        Ok(v) => match v.trim().parse::<u64>() {
            // 0 means "never", matching --no-idle-exit.
            Ok(0) => None,
            Ok(n) => Some(n),
            // An unparseable value falls back to the default rather than
            // silently disabling the timer -- failing open on a lifetime
            // control is how daemons leak.
            Err(_) => Some(DEFAULT_IDLE_EXIT_MINS),
        },
        Err(_) => Some(DEFAULT_IDLE_EXIT_MINS),
    }
}
