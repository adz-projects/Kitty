//! What a host tells this server: where its own storage goes, which
//! directories it may touch, and which optional tools it offers.
//!
//! A stdio binary gets these from its environment, which its host sets per
//! child at spawn. A host that links this crate in-process (the daemon on
//! Android, where `exec()` of a bundled binary is refused) has no per-server
//! environment to hand over: every server it runs shares one process. It used
//! to fall back on that process's environment, which meant one app's settings
//! applied to every app's servers and a changed setting (turning Brave off,
//! turning visualizations on) did not arrive until the app was relaunched.
//! [`crate::serve_in_process`] now takes this struct explicitly, and the stdio
//! `main` builds the same struct from its environment, so both hosting modes
//! run one code path.
//!
//! **How the value reaches the code that needs it.** The tool
//! implementations are plain functions several calls below the MCP handler,
//! many of them run on the blocking pool. Rather than thread a parameter
//! through every one of them, the handler runs each call inside [`scope`]
//! (a task-local), and [`spawn_blocking`] carries the scoped value onto the
//! blocking thread. [`current`] reads whichever is in effect. Code running
//! outside any server (unit tests, a library caller) sees the process
//! environment, read fresh, which is what it always saw.
//!
//! Deliberately duplicated in `kitty-web` and `kitty-wasm` rather than shared
//! through a common crate: these ship as separate frozen binaries (see
//! `envelope.rs`'s duplication note).

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use crate::paths::{ALLOWED_DIRS_ENV, ALLOWED_DIRS_FILE_ENV, PLUGIN_HOME_ENV};

/// The variable that turns on the three visualization tools.
pub const VIZ_ENABLED_ENV: &str = "KITTY_VIZ_ENABLED";

/// Everything this server reads from its host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InProcessConfig {
    /// This server's own storage root (scratchpad, document cache), and the
    /// fallback authorization root. `None` when no home can be determined,
    /// in which case every path check rejects. See `paths::home_dir`.
    pub plugin_home: Option<PathBuf>,
    /// Directories this server may read and write beyond `plugin_home`.
    pub allowed_dirs: Vec<PathBuf>,
    /// A JSON array of further allowed directories, re-read as it changes.
    pub allowed_dirs_file: Option<PathBuf>,
    /// Whether the three visualization tools are advertised.
    pub viz_enabled: bool,
}

impl InProcessConfig {
    /// Build from a key lookup, using the same variable names the stdio
    /// binary reads from its environment.
    ///
    /// The home falls back from `KITTY_PLUGIN_HOME` to `USERPROFILE`/`HOME`
    /// and then to the OS's own answer, so a host that sets nothing gets the
    /// user's home directory, as before.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let non_empty = |key: &str| get(key).filter(|v| !v.trim().is_empty());
        let plugin_home = [PLUGIN_HOME_ENV, "USERPROFILE", "HOME"]
            .into_iter()
            .find_map(|key| non_empty(key).map(PathBuf::from))
            .or_else(dirs::home_dir);
        let allowed_dirs = non_empty(ALLOWED_DIRS_ENV)
            // `split_paths`, not a split on ';' or ':' - the latter would cut
            // every Windows path in half at its drive letter.
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
            viz_enabled: get(VIZ_ENABLED_ENV).as_deref() == Some("1"),
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
/// the blocking thread. Every blocking hop in a tool call goes through this;
/// a bare `spawn_blocking` would silently fall back to the process
/// environment.
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

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_map_round_trips_every_field() {
        let (a, b) = (std::env::temp_dir().join("a"), std::env::temp_dir().join("b"));
        let dirs = std::env::join_paths([a.clone(), PathBuf::from("."), b.clone()]).unwrap();
        let config = InProcessConfig::from_map(&map(&[
            (PLUGIN_HOME_ENV, "home"),
            (ALLOWED_DIRS_ENV, dirs.to_str().unwrap()),
            (ALLOWED_DIRS_FILE_ENV, "grants.json"),
            (VIZ_ENABLED_ENV, "1"),
        ]));
        assert_eq!(
            config,
            InProcessConfig {
                plugin_home: Some(PathBuf::from("home")),
                allowed_dirs: vec![a, b],
                allowed_dirs_file: Some(PathBuf::from("grants.json")),
                viz_enabled: true,
            }
        );
    }

    #[test]
    fn an_empty_map_still_finds_a_home_and_enables_nothing() {
        let config = InProcessConfig::from_map(&HashMap::new());
        assert_eq!(config.plugin_home, dirs::home_dir());
        assert!(config.allowed_dirs.is_empty());
        assert!(config.allowed_dirs_file.is_none());
        assert!(!config.viz_enabled);
    }

    #[tokio::test]
    async fn the_scoped_value_reaches_blocking_threads_and_does_not_leak() {
        let config = Arc::new(InProcessConfig {
            plugin_home: Some(PathBuf::from("scoped-home")),
            ..Default::default()
        });
        let seen = scope(config.clone(), async {
            spawn_blocking(|| current().plugin_home.clone())
                .await
                .unwrap()
        })
        .await;
        assert_eq!(seen, Some(PathBuf::from("scoped-home")));

        // Outside the scope, a blocking thread sees the process default again.
        let after = spawn_blocking(|| current().plugin_home.clone())
            .await
            .unwrap();
        assert_ne!(after, Some(PathBuf::from("scoped-home")));
    }

    /// Two servers in one process, configured differently, each advertise
    /// their own tool surface - the in-process case, where the environment
    /// cannot tell them apart.
    #[test]
    fn servers_in_one_process_follow_their_own_config() {
        use crate::server::KittyToolsServer;
        let with = KittyToolsServer::with_config(InProcessConfig {
            viz_enabled: true,
            ..Default::default()
        });
        let without = KittyToolsServer::with_config(InProcessConfig::default());
        assert!(with
            .tool_names()
            .iter()
            .any(|n| n == "generate_accessible_chart"));
        assert!(!without
            .tool_names()
            .iter()
            .any(|n| n == "generate_accessible_chart"));
    }
}
