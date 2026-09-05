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
/// Sized for several apps rather than one user. V1 pinned this at 4, which
/// was right for a single-user desktop daemon; with three frontends each
/// running concurrent turns, four workers is the bottleneck before the
/// provider is. Clamped at 8 so a many-core machine does not spawn more
/// runtime threads than the SQLite pool and provider slots can feed.
const MAX_WORKER_THREADS: usize = 8;
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

/// `bigtiny2-daemon import --from <v1.db> [--app-id kitty] [--key <k>]
///                        [--encryption-key <hex>] [--pathway-from <path>]`
///
/// Runs instead of the server and exits. Separated from `async_main` because
/// an import must not start a listener, write a handshake file, or arm the
/// idle-exit timer — it is a one-shot data migration that happens to need the
/// daemon's own migration chain and crypto.
async fn run_import(argv: &[String]) -> ! {
    let flag = |name: &str| -> Option<String> {
        argv.iter()
            .position(|a| a == name)
            .and_then(|i| argv.get(i + 1))
            .cloned()
    };

    let Some(source) = flag("--from") else {
        eprintln!("import requires --from <path to V1 bigtiny.db>");
        std::process::exit(2);
    };
    let app_id = flag("--app-id").unwrap_or_else(|| "kitty".to_string());
    let display_name = flag("--display-name").unwrap_or_else(|| "Kitty".to_string());

    let data_dir = resolve_data_dir();
    // The key must be in force *before* the pool is opened, so the
    // decryptability check reads provider rows with the same cipher the
    // running daemon will use. `--encryption-key` is how V1's key is carried
    // across; without it the import still succeeds and reports how many
    // providers it could not read.
    // `BIGTINY_ENCRYPTION_KEY` is V1's variable name, and this is the one
    // command whose entire job is to read V1's world -- so it is accepted here
    // even though the running daemon reads the V2-scoped name. The explicit
    // flag wins over both.
    let env_key = flag("--encryption-key")
        .or_else(|| std::env::var("BIGTINYV2_ENCRYPTION_KEY").ok())
        .or_else(|| std::env::var("BIGTINY_ENCRYPTION_KEY").ok())
        .filter(|k| !k.trim().is_empty());
    if let Err(e) = bigtiny2::crypto::init(&data_dir, env_key.as_deref()) {
        eprintln!("could not initialize encryption: {e}");
        std::process::exit(1);
    }
    // Adopt V1's key permanently, not just for this process. Without this the
    // import verifies that provider rows decrypt and then leaves a daemon that
    // cannot read them on its next start -- the exact silent failure the
    // summary's warning exists to prevent, arriving later and looking like a
    // provider outage instead of a migration mistake.
    let mut adopted = false;
    if let Some(hex) = env_key.as_deref() {
        match bigtiny2::crypto::adopt_key(&data_dir, hex) {
            Ok(true) => adopted = true,
            Ok(false) => {}
            Err(e) => {
                eprintln!("could not store the carried encryption key: {e}");
                std::process::exit(1);
            }
        }
    }

    let dest = data_dir.join("bigtiny.db");
    let key = flag("--key").unwrap_or_else(|| {
        // A fresh key is generated rather than prompted for: the app has to
        // store it somewhere anyway, and printing it once matches what
        // registration does.
        use rand::Rng;
        let bytes: [u8; 32] = rand::thread_rng().gen();
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    });

    match bigtiny2::import::import_v1(
        std::path::Path::new(&source),
        &dest,
        &app_id,
        &display_name,
        &key,
    )
    .await
    {
        Ok(summary) => {
            println!("{}", summary.render());
            if let Some(pathway_src) = flag("--pathway-from") {
                match bigtiny2::import::import_pathway(
                    std::path::Path::new(&pathway_src),
                    &data_dir,
                    &app_id,
                ) {
                    Some(p) => println!("  pathway graph -> {}", p.display()),
                    None => println!("  no pathway graph imported (source not found)"),
                }
            }
            if adopted {
                println!("  encryption key adopted -> {}", data_dir.join("encryption.key").display());
            }
            println!("\n  API key for {app_id}: {key}");
            println!("  Store this now — it is not recoverable from the database.");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("import failed: {e}");
            std::process::exit(1);
        }
    }
}

async fn async_main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("import") {
        run_import(&argv).await;
    }

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
        // Note this no longer means "run unauthenticated", which is what it
        // meant in V1. `auth_middleware` requires a valid app key on every
        // `/api/*` route except `/api/health` and the registration route,
        // unconditionally -- there is no anonymous mode to fall into, because
        // per-app identity is what the whole tenancy design rests on. All
        // `secret` still does here is seed the registration token, and a
        // random one is generated when it is absent.
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
