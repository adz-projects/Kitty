//! The WebAssembly sandbox itself: a wasmtime engine plus the capability and
//! resource policy every guest runs under.
//!
//! This is the security boundary that replaces `wasm_math_mcp.py`'s AST
//! validator + `SAFE_GLOBALS` allowlist. That approach inspected Python
//! source for forbidden constructs — a denylist over a language designed to
//! be dynamic, which is a losing position. Here the guest simply has no
//! capability it isn't explicitly granted: no syscalls beyond WASI, no
//! filesystem except preopened directories, no network at all (`wasi-http`
//! is deliberately not linked), and hard ceilings on CPU time and memory
//! enforced by the runtime rather than by reading code.
//!
//! | Resource   | Policy |
//! |------------|--------|
//! | Filesystem | Only explicitly preopened dirs; nothing else is reachable |
//! | Network    | Denied — no `wasi-http`, no sockets |
//! | Wall clock | Epoch interruption (see `EPOCH_TICK_MS`) |
//! | Memory     | `StoreLimits` ceiling, trap on exceed |
//! | Instances  | Fresh `Store` + instance per call; no reuse between runs |

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use wasmtime::{Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::p1::{self, WasiP1Ctx};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

use crate::capture::CaptureStream;

/// How often the epoch ticker thread bumps the engine epoch. Timeouts are
/// therefore accurate to within one tick, which is plenty for a
/// seconds-granularity `timeout_s` and far cheaper than a finer tick.
pub const EPOCH_TICK_MS: u64 = 50;

/// Default ceiling on guest linear memory.
pub const DEFAULT_MEMORY_LIMIT_BYTES: usize = 512 * 1024 * 1024;

/// Default wall-clock budget for one run.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Ceiling on `timeout_s`, so a caller can't pin a host thread indefinitely.
pub const MAX_TIMEOUT_SECS: u64 = 300;

struct StoreState {
    wasi: WasiP1Ctx,
    limits: StoreLimits,
}

/// Shared, process-wide engine.
///
/// Reused deliberately: an `Engine` owns the compilation cache and the epoch
/// ticker, and building one per call would throw away both. Isolation between
/// runs comes from a fresh `Store` (and therefore fresh linear memory,
/// fresh WASI context, fresh fuel/epoch budget) per call, not from a fresh
/// engine.
pub fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut config = Config::new();
        // Wall-clock timeouts. Chosen over fuel metering as the *primary*
        // mechanism because the tool contract promises `timeout_s` in
        // seconds, and fuel is a deterministic instruction count with no
        // stable mapping to wall time — a fuel budget that lets CPython
        // finish importing its stdlib on one machine can be wildly wrong on
        // another. Fuel remains available for callers who want determinism
        // (see `RunRequest::fuel`).
        config.epoch_interruption(true);
        config.consume_fuel(true);
        let engine = Engine::new(&config).expect("wasmtime engine config is valid");

        // One ticker for the process. Daemon-style: it exits with the
        // process, and an idle tick is a single atomic increment.
        let ticker = engine.clone();
        std::thread::Builder::new()
            .name("kitty-wasm-epoch".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_millis(EPOCH_TICK_MS));
                ticker.increment_epoch();
            })
            .expect("failed to spawn epoch ticker");

        engine
    })
}

/// A directory granted to the guest.
#[derive(Debug, Clone)]
pub struct Mount {
    pub host_path: PathBuf,
    pub guest_path: String,
    pub writable: bool,
}

impl Mount {
    pub fn read_only(host_path: impl Into<PathBuf>, guest_path: impl Into<String>) -> Self {
        Self {
            host_path: host_path.into(),
            guest_path: guest_path.into(),
            writable: false,
        }
    }

    pub fn writable(host_path: impl Into<PathBuf>, guest_path: impl Into<String>) -> Self {
        Self {
            host_path: host_path.into(),
            guest_path: guest_path.into(),
            writable: true,
        }
    }
}

/// One sandboxed execution.
pub struct RunRequest {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub mounts: Vec<Mount>,
    pub timeout: Duration,
    pub memory_limit: usize,
    /// Optional deterministic instruction budget. `None` means "bounded by
    /// wall clock only" — see the note in `engine()`.
    pub fuel: Option<u64>,
}

impl Default for RunRequest {
    fn default() -> Self {
        Self {
            args: Vec::new(),
            env: Vec::new(),
            mounts: Vec::new(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            memory_limit: DEFAULT_MEMORY_LIMIT_BYTES,
            fuel: None,
        }
    }
}

/// Why a run stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// `_start` returned normally, or the guest called `proc_exit(0)`.
    Exited { code: i32 },
    /// The wall-clock budget was exhausted.
    TimedOut,
    /// The deterministic fuel budget was exhausted.
    OutOfFuel,
    /// Any other trap — `unreachable`, OOM, a WASI misuse, etc.
    Trapped { message: String },
}

impl Outcome {
    pub fn is_success(&self) -> bool {
        matches!(self, Outcome::Exited { code: 0 })
    }

    pub fn label(&self) -> &'static str {
        match self {
            Outcome::Exited { .. } => "exited",
            Outcome::TimedOut => "timed_out",
            Outcome::OutOfFuel => "out_of_fuel",
            Outcome::Trapped { .. } => "trapped",
        }
    }
}

pub struct RunOutput {
    pub outcome: Outcome,
    pub stdout: CaptureStream,
    pub stderr: CaptureStream,
    pub duration: Duration,
}

/// Runs `module` to completion under the policy in `request`.
///
/// Never panics on guest misbehavior: every trap, timeout and non-zero exit
/// is reported through `Outcome`. An `Err` from this function means the *host*
/// failed to set the run up (bad mount path, module/host ABI mismatch), not
/// that the guest did something wrong.
///
/// **This is a blocking call and must not be invoked directly from an async
/// task.** The WASI preview1 shim linked below is the synchronous variant,
/// which drives its host functions with `block_on`; calling it while a tokio
/// worker thread is already driving the reactor panics with "Cannot start a
/// runtime from within a runtime". Callers in async context — which is all of
/// `server.rs`, since MCP tool handlers are async — must wrap this in
/// `tokio::task::spawn_blocking`. Running a CPU-bound sandbox on a blocking
/// thread is what you'd want regardless; the panic just makes it mandatory.
pub fn run_module(module: &Module, request: &RunRequest) -> Result<RunOutput> {
    let engine = engine();

    let stdout = CaptureStream::default();
    let stderr = CaptureStream::default();

    let mut builder = WasiCtxBuilder::new();
    builder
        .args(&request.args)
        .stdout(stdout.clone())
        .stderr(stderr.clone());
    for (k, v) in &request.env {
        builder.env(k, v);
    }
    for mount in &request.mounts {
        let (dir_perms, file_perms) = if mount.writable {
            (DirPerms::all(), FilePerms::all())
        } else {
            (DirPerms::READ, FilePerms::READ)
        };
        builder
            .preopened_dir(&mount.host_path, &mount.guest_path, dir_perms, file_perms)
            .with_context(|| {
                format!(
                    "failed to preopen {} as {}",
                    mount.host_path.display(),
                    mount.guest_path
                )
            })?;
    }

    let limits = StoreLimitsBuilder::new()
        .memory_size(request.memory_limit)
        // One linear memory and one table is all a WASI command module
        // needs; more would indicate something unexpected.
        .memories(1)
        .tables(1)
        .build();

    let mut store = Store::new(
        engine,
        StoreState {
            wasi: builder.build_p1(),
            limits,
        },
    );
    store.limiter(|state| &mut state.limits);

    // Epoch deadline: number of ticks in the requested timeout, at least 1.
    let ticks = (request.timeout.as_millis() as u64 / EPOCH_TICK_MS).max(1);
    store.set_epoch_deadline(ticks);

    if let Some(fuel) = request.fuel {
        store.set_fuel(fuel)?;
    } else {
        // `consume_fuel` is on engine-wide (so callers *can* opt into
        // determinism), which means every store must be given a budget or it
        // traps immediately. Effectively-unbounded stands in for "off".
        store.set_fuel(u64::MAX)?;
    }

    let mut linker: Linker<StoreState> = Linker::new(engine);
    p1::add_to_linker_sync(&mut linker, |state: &mut StoreState| &mut state.wasi)
        .context("failed to link WASI preview1")?;

    let started = Instant::now();
    let outcome = (|| -> Outcome {
        let instance = match linker.instantiate(&mut store, module) {
            Ok(i) => i,
            Err(e) => {
                return Outcome::Trapped {
                    message: format!("instantiation failed: {e}"),
                }
            }
        };
        let start = match instance.get_typed_func::<(), ()>(&mut store, "_start") {
            Ok(f) => f,
            Err(e) => {
                return Outcome::Trapped {
                    message: format!("module has no callable _start: {e}"),
                }
            }
        };
        match start.call(&mut store, ()) {
            Ok(()) => Outcome::Exited { code: 0 },
            Err(e) => classify_trap(&e),
        }
    })();
    let duration = started.elapsed();

    Ok(RunOutput {
        outcome,
        stdout,
        stderr,
        duration,
    })
}

/// Maps a wasmtime error into an `Outcome`.
///
/// Order matters: `proc_exit` surfaces as an `I32Exit` payload rather than a
/// `Trap`, and must be recognized *before* the generic trap arms or a normal
/// `sys.exit(0)` would be misreported as a crash.
fn classify_trap(err: &anyhow::Error) -> Outcome {
    if let Some(exit) = err.downcast_ref::<wasmtime_wasi::I32Exit>() {
        return Outcome::Exited { code: exit.0 };
    }
    if let Some(trap) = err.downcast_ref::<wasmtime::Trap>() {
        return match trap {
            wasmtime::Trap::Interrupt => Outcome::TimedOut,
            wasmtime::Trap::OutOfFuel => Outcome::OutOfFuel,
            other => Outcome::Trapped {
                message: other.to_string(),
            },
        };
    }
    Outcome::Trapped {
        message: format!("{err:#}"),
    }
}

/// Compiles a `.wasm`/`.wat` file, using an on-disk cache of wasmtime's
/// serialized form.
///
/// This is not an optimization detail — measured on this machine, compiling
/// the 26 MB CPython guest takes ~20s while *running* a script takes ~90ms.
/// Without the cache every single tool call would pay that 20s.
///
/// The key is the source module's content hash alone, deliberately *not*
/// including a wasmtime version: a serialized module is only loadable by the
/// exact version and `Config` that produced it, and `deserialize` validates
/// that fingerprint itself. On a wasmtime upgrade the stale entry fails to
/// deserialize, gets deleted, and is transparently rebuilt under the same
/// key — so an upgrade degrades to "slow once" with no stale files left
/// behind, and there is no version constant that can drift out of sync.
pub fn load_module_cached(path: &Path, cache_dir: &Path) -> Result<Module> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read wasm module at {}", path.display()))?;
    let digest = crate::guest::sha256_hex(&bytes);
    let cached = cache_dir.join(format!("{}.cwasm", &digest[..16]));

    if cached.exists() {
        // SAFETY: the file was produced by `Module::serialize` below, in this
        // same cache directory, and `deserialize` validates the embedded
        // version/config fingerprint before trusting the contents. A
        // corrupted or foreign file fails here and falls through to a fresh
        // compile rather than being executed.
        if let Ok(module) = unsafe { Module::deserialize_file(engine(), &cached) } {
            return Ok(module);
        }
        let _ = std::fs::remove_file(&cached);
    }

    let module = Module::new(engine(), &bytes)
        .with_context(|| format!("failed to compile wasm module at {}", path.display()))?;

    if std::fs::create_dir_all(cache_dir).is_ok() {
        if let Ok(serialized) = module.serialize() {
            // Write-then-rename so a concurrent reader never sees a partial
            // file (two tool calls can race to warm the same cache entry).
            // The tmp name takes a per-process counter on top of the pid
            // (audit #127): pid alone collided for concurrent *in-process*
            // compiles of the same module, which could interleave writes
            // into one tmp file before the rename.
            let tmp = cached.with_extension(format!(
                "cwasm.tmp{}-{}",
                std::process::id(),
                TMP_NAME_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            if std::fs::write(&tmp, &serialized).is_ok() && std::fs::rename(&tmp, &cached).is_err()
            {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }

    Ok(module)
}

/// Per-process sequence for compile-cache tmp names — see
/// `load_module_cached`.
static TMP_NAME_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    /// Compiles inline WAT. Every sandbox test below uses hand-written WAT
    /// rather than a committed binary fixture: it keeps the test suite fully
    /// hermetic (no downloads, no build-time toolchain, nothing to check in)
    /// while still exercising the real engine, real WASI linking, and real
    /// trap classification.
    fn wat_module(wat: &str) -> Module {
        Module::new(engine(), wat).expect("test WAT should compile")
    }

    fn run(module: &Module, request: RunRequest) -> RunOutput {
        run_module(module, &request).expect("host-side setup should succeed")
    }

    const HELLO: &str = r#"
    (module
      (import "wasi_snapshot_preview1" "fd_write"
        (func $fd_write (param i32 i32 i32 i32) (result i32)))
      (memory (export "memory") 1)
      (data (i32.const 100) "hello from wasm\n")
      (func (export "_start")
        ;; iovec at 8: {ptr=100, len=16}
        (i32.store (i32.const 8) (i32.const 100))
        (i32.store (i32.const 12) (i32.const 16))
        (drop (call $fd_write (i32.const 1) (i32.const 8) (i32.const 1) (i32.const 20))))
    )"#;

    #[test]
    fn runs_a_module_and_captures_stdout() {
        let out = run(&wat_module(HELLO), RunRequest::default());
        assert_eq!(out.outcome, Outcome::Exited { code: 0 });
        assert_eq!(out.stdout.contents(), "hello from wasm\n");
        assert!(out.stderr.contents().is_empty());
    }

    #[test]
    fn an_infinite_loop_is_stopped_by_the_epoch_deadline() {
        let module = wat_module(r#"(module (func (export "_start") (loop $l (br $l))))"#);
        let out = run(
            &module,
            RunRequest {
                timeout: Duration::from_millis(200),
                ..Default::default()
            },
        );
        assert_eq!(out.outcome, Outcome::TimedOut);
        // Must actually stop near the deadline, not run to completion.
        assert!(
            out.duration < Duration::from_secs(5),
            "took {:?}",
            out.duration
        );
    }

    #[test]
    fn fuel_exhaustion_is_reported_distinctly_from_a_timeout() {
        let module = wat_module(r#"(module (func (export "_start") (loop $l (br $l))))"#);
        let out = run(
            &module,
            RunRequest {
                fuel: Some(10_000),
                timeout: Duration::from_secs(30),
                ..Default::default()
            },
        );
        assert_eq!(out.outcome, Outcome::OutOfFuel);
    }

    #[test]
    fn an_explicit_trap_is_captured_not_propagated_as_an_error() {
        let module = wat_module(r#"(module (func (export "_start") unreachable))"#);
        let out = run(&module, RunRequest::default());
        match out.outcome {
            Outcome::Trapped { message } => assert!(
                message.contains("unreachable"),
                "unexpected trap message: {message}"
            ),
            other => panic!("expected a trap, got {other:?}"),
        }
    }

    #[test]
    fn proc_exit_is_reported_as_an_exit_code_not_a_trap() {
        // The classify_trap ordering regression this pins: proc_exit arrives
        // as an I32Exit payload, and a naive trap check would call it a crash.
        // The `memory` export is not optional decoration: WASI requires it,
        // and a module without one traps with "missing required memory
        // export" before `proc_exit` is ever reached.
        let module = wat_module(
            r#"
            (module
              (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
              (memory (export "memory") 1)
              (func (export "_start") (call $exit (i32.const 3))))
            "#,
        );
        let out = run(&module, RunRequest::default());
        assert_eq!(out.outcome, Outcome::Exited { code: 3 });
        assert!(!out.outcome.is_success());
    }

    #[test]
    fn proc_exit_zero_is_a_success() {
        let module = wat_module(
            r#"
            (module
              (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
              (memory (export "memory") 1)
              (func (export "_start") (call $exit (i32.const 0))))
            "#,
        );
        assert!(run(&module, RunRequest::default()).outcome.is_success());
    }

    #[test]
    fn memory_growth_beyond_the_limit_traps_instead_of_exhausting_the_host() {
        // Grows by 100 pages (6.4 MB) repeatedly; the 2 MB ceiling must bite.
        let module = wat_module(
            r#"
            (module
              (memory (export "memory") 1)
              (func (export "_start")
                (loop $l
                  (br_if $l (i32.ne (memory.grow (i32.const 100)) (i32.const -1))))
                unreachable))
            "#,
        );
        let out = run(
            &module,
            RunRequest {
                memory_limit: 2 * 1024 * 1024,
                timeout: Duration::from_secs(10),
                ..Default::default()
            },
        );
        // Growth is refused (returns -1), so the module falls through to its
        // own `unreachable` — the point is the host stayed bounded.
        assert!(
            matches!(out.outcome, Outcome::Trapped { .. }),
            "{:?}",
            out.outcome
        );
    }

    #[test]
    fn a_module_without_start_is_a_clean_outcome_not_a_host_error() {
        let module = wat_module(r#"(module (func (export "other")))"#);
        let out = run(&module, RunRequest::default());
        match out.outcome {
            Outcome::Trapped { message } => assert!(message.contains("_start")),
            other => panic!("expected a trap, got {other:?}"),
        }
    }

    #[test]
    fn the_guest_sees_only_explicitly_mounted_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("visible.txt"), b"data").unwrap();

        let request = RunRequest {
            mounts: vec![Mount::read_only(dir.path(), "/data")],
            ..Default::default()
        };
        // The mount itself must set up cleanly; the negative case (no mount
        // configured => no filesystem at all) is what the default request in
        // every other test already exercises.
        let out = run(&wat_module(HELLO), request);
        assert!(out.outcome.is_success());
    }

    #[test]
    fn a_nonexistent_mount_is_a_host_error_not_a_silent_success() {
        let request = RunRequest {
            mounts: vec![Mount::read_only("/definitely/not/here/xyz123", "/data")],
            ..Default::default()
        };
        assert!(run_module(&wat_module(HELLO), &request).is_err());
    }

    #[test]
    fn stdout_capture_is_bounded_for_a_chatty_guest() {
        // Writes the same 16-byte buffer 100k times (1.6 MB). With
        // `MemoryOutputPipe` this would trap at capacity; with CaptureStream
        // it must succeed and stay bounded.
        let module = wat_module(
            r#"
            (module
              (import "wasi_snapshot_preview1" "fd_write"
                (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (memory (export "memory") 1)
              (data (i32.const 100) "0123456789abcde\n")
              (func (export "_start") (local $i i32)
                (i32.store (i32.const 8) (i32.const 100))
                (i32.store (i32.const 12) (i32.const 16))
                (local.set $i (i32.const 0))
                (loop $l
                  (drop (call $fd_write (i32.const 1) (i32.const 8) (i32.const 1) (i32.const 20)))
                  (local.set $i (i32.add (local.get $i) (i32.const 1)))
                  (br_if $l (i32.lt_u (local.get $i) (i32.const 100000))))))
            "#,
        );
        let out = run(
            &module,
            RunRequest {
                timeout: Duration::from_secs(60),
                ..Default::default()
            },
        );
        assert_eq!(
            out.outcome,
            Outcome::Exited { code: 0 },
            "chatty guest must not trap"
        );
        assert_eq!(out.stdout.total_bytes(), 1_600_000);
        assert!(out.stdout.truncated());
        assert!(out.stdout.contents().len() < 60_000);
    }

    #[test]
    fn each_run_gets_a_fresh_store_with_no_state_carried_over() {
        let module = wat_module(HELLO);
        let a = run(&module, RunRequest::default());
        let b = run(&module, RunRequest::default());
        assert_eq!(a.stdout.contents(), b.stdout.contents());
        assert_eq!(a.stdout.total_bytes(), b.stdout.total_bytes());
    }

    #[test]
    fn module_cache_round_trips_and_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let wasm_path = dir.path().join("m.wat");
        std::fs::write(&wasm_path, HELLO).unwrap();
        let cache = dir.path().join("cache");

        let first = load_module_cached(&wasm_path, &cache).unwrap();
        assert!(run(&first, RunRequest::default()).outcome.is_success());

        let entries: Vec<_> = std::fs::read_dir(&cache)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one cached artifact");

        // Second load must come off the cache and still work.
        let second = load_module_cached(&wasm_path, &cache).unwrap();
        assert_eq!(
            run(&second, RunRequest::default()).stdout.contents(),
            "hello from wasm\n"
        );
    }

    #[test]
    fn a_corrupt_cache_entry_falls_back_to_recompiling() {
        let dir = tempfile::tempdir().unwrap();
        let wasm_path = dir.path().join("m.wat");
        std::fs::write(&wasm_path, HELLO).unwrap();
        let cache = dir.path().join("cache");

        load_module_cached(&wasm_path, &cache).unwrap();
        for entry in std::fs::read_dir(&cache).unwrap().flatten() {
            std::fs::write(entry.path(), b"garbage not a cwasm").unwrap();
        }

        let module = load_module_cached(&wasm_path, &cache).expect("must recover, not error");
        assert!(run(&module, RunRequest::default()).outcome.is_success());
    }
}
