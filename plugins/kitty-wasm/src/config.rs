//! What a host tells this server: where its own storage goes, which
//! directories a workspace may be mounted from, and where the Python guest is.
//!
//! A stdio binary gets these from its environment, which its host sets per
//! child at spawn. A host that links this crate in-process (the daemon on
//! Android) has no per-server environment to hand over: every server it runs
//! shares one process, and a value set on that process applied to every app's
//! servers at once. [`crate::serve_in_process`] now takes this struct
//! explicitly, and the stdio `main` builds the same struct from its
//! environment, so both hosting modes run one code path.
//!
//! The handler runs each tool call inside [`scope`] (a task-local), and
//! [`spawn_blocking`] carries the scoped value onto the blocking pool, where
//! the sandbox runs; [`current`] reads whichever is in effect. Code running
//! outside any server (unit tests, a library caller) sees the process
//! environment, read fresh.
//!
//! Duplicated from `kitty-tools`' `config.rs` rather than shared, for the
//! reason `paths.rs` gives.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use crate::paths::{ALLOWED_DIRS_ENV, ALLOWED_DIRS_FILE_ENV, PLUGIN_HOME_ENV};

/// An explicit CPython guest to use instead of the managed download.
pub const WASM_PYTHON_ENV: &str = "KITTY_WASM_PYTHON";

/// Where this crate keeps its guests, compiled modules and run scratch,
/// instead of `<home>/.kitty-wasm`.
pub const WASM_DATA_DIR_ENV: &str = "KITTY_WASM_DATA_DIR";

/// Everything this server reads from its host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InProcessConfig {
    /// This server's storage root and fallback mount root. `None` when no
    /// home can be determined, in which case every mount is refused.
    pub plugin_home: Option<PathBuf>,
    /// Directories a workspace may be mounted from beyond `plugin_home`.
    pub allowed_dirs: Vec<PathBuf>,
    /// A JSON array of further allowed directories, re-read as it changes.
    pub allowed_dirs_file: Option<PathBuf>,
    /// An explicit CPython guest. Set but missing is an error, not a reason
    /// to fall back to the managed copy (see `guest::find_python_guest`).
    pub wasm_python: Option<PathBuf>,
    /// Overrides `<plugin_home>/.kitty-wasm`.
    pub wasm_data_dir: Option<PathBuf>,
}

impl InProcessConfig {
    /// Build from a key lookup, using the same variable names the stdio
    /// binary reads from its environment.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let non_empty = |key: &str| get(key).filter(|v| !v.trim().is_empty());
        let plugin_home = [PLUGIN_HOME_ENV, "USERPROFILE", "HOME"]
            .into_iter()
            .find_map(|key| non_empty(key).map(PathBuf::from))
            .or_else(dirs::home_dir);
        let allowed_dirs = non_empty(ALLOWED_DIRS_ENV)
            // `split_paths`, not a split on ':' - that would cut a Windows
            // path at its drive letter.
            // Relative entries are dropped: a root is canonicalized against
            // the working directory, so `.` would become `/` on a host whose
            // working directory is the filesystem root (an Android app).
            .map(|raw| {
                std::env::split_paths(&raw)
                    .filter(|p| p.is_absolute())
                    .collect()
            })
            .unwrap_or_default();
        Self {
            plugin_home,
            allowed_dirs,
            allowed_dirs_file: non_empty(ALLOWED_DIRS_FILE_ENV).map(PathBuf::from),
            // Present-but-blank still counts as set, as it always has: the
            // guest resolver reports it rather than silently ignoring it.
            wasm_python: get(WASM_PYTHON_ENV).map(PathBuf::from),
            wasm_data_dir: non_empty(WASM_DATA_DIR_ENV).map(PathBuf::from),
        }
    }

    /// Build from this process's environment - the stdio binary's source.
    pub fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Build from an explicit map of the same variables - an in-process
    /// host's source. Nothing is read from the process environment except
    /// the OS home-directory fallback.
    pub fn from_map(map: &HashMap<String, String>) -> Self {
        Self::from_lookup(|key| map.get(key).cloned())
    }
}

tokio::task_local! {
    static SCOPED: Arc<InProcessConfig>;
}

thread_local! {
    static ON_BLOCKING_THREAD: RefCell<Option<Arc<InProcessConfig>>> = const { RefCell::new(None) };
}

/// The configuration in effect for the code running now.
pub fn current() -> Arc<InProcessConfig> {
    if let Ok(config) = SCOPED.try_with(Arc::clone) {
        return config;
    }
    if let Some(config) = ON_BLOCKING_THREAD.with(|slot| slot.borrow().clone()) {
        return config;
    }
    Arc::new(InProcessConfig::from_env())
}

/// Run `fut` with `config` in effect.
pub async fn scope<F: Future>(config: Arc<InProcessConfig>, fut: F) -> F::Output {
    SCOPED.scope(config, fut).await
}

/// `tokio::task::spawn_blocking`, carrying the configuration in effect onto
/// the blocking thread.
pub fn spawn_blocking<F, R>(f: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let config = current();
    tokio::task::spawn_blocking(move || {
        /// Restores the thread's previous value even if `f` panics - pool
        /// threads are reused, and a leftover would leak into the next call.
        struct Restore(Option<Arc<InProcessConfig>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                let previous = self.0.take();
                ON_BLOCKING_THREAD.with(|slot| *slot.borrow_mut() = previous);
            }
        }
        let _restore = Restore(ON_BLOCKING_THREAD.with(|slot| slot.replace(Some(config))));
        f()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_map_round_trips_every_field() {
        let (a, b) = (std::env::temp_dir().join("a"), std::env::temp_dir().join("b"));
        let dirs = std::env::join_paths([a.clone(), PathBuf::from("."), b.clone()]).unwrap();
        let map: HashMap<String, String> = [
            (PLUGIN_HOME_ENV, "home"),
            (ALLOWED_DIRS_ENV, dirs.to_str().unwrap()),
            (ALLOWED_DIRS_FILE_ENV, "grants.json"),
            (WASM_PYTHON_ENV, "py.wasm"),
            (WASM_DATA_DIR_ENV, "data"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(
            InProcessConfig::from_map(&map),
            InProcessConfig {
                plugin_home: Some(PathBuf::from("home")),
                allowed_dirs: vec![a, b],
                allowed_dirs_file: Some(PathBuf::from("grants.json")),
                wasm_python: Some(PathBuf::from("py.wasm")),
                wasm_data_dir: Some(PathBuf::from("data")),
            }
        );
    }

    #[tokio::test]
    async fn the_scoped_value_reaches_blocking_threads() {
        let config = Arc::new(InProcessConfig {
            wasm_data_dir: Some(PathBuf::from("scoped")),
            ..Default::default()
        });
        let seen = scope(config, async {
            spawn_blocking(crate::guest::data_dir).await.unwrap()
        })
        .await;
        assert_eq!(seen, PathBuf::from("scoped"));
    }
}
