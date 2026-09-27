//! What a host tells this server: where its own storage goes and which
//! search credentials it holds.
//!
//! A stdio binary gets these from its environment, which its host sets per
//! child at spawn. A host that links this crate in-process (the daemon on
//! Android) has no per-server environment to hand over: every server it runs
//! shares one process. It used to fall back on that process's environment,
//! which is how turning Brave off in Settings left `BRAVE_API_KEY` in place
//! and Brave still in use until the app was relaunched.
//! [`crate::serve_in_process`] now takes this struct explicitly, and the stdio
//! `main` builds the same struct from its environment, so both hosting modes
//! run one code path.
//!
//! The handler runs each tool call inside [`scope`] (a task-local), and
//! [`spawn_blocking`] carries the scoped value onto the blocking pool;
//! [`current`] reads whichever is in effect. Code running outside any server
//! (unit tests, a library caller) sees the process environment, read fresh.
//!
//! Duplicated from `kitty-tools`' `config.rs` rather than shared, for the
//! reason `paths.rs` gives.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use crate::paths::PLUGIN_HOME_ENV;

/// The variable holding a Brave Search API key. Unset or empty means the
/// key-free DuckDuckGo/Bing pair only.
pub const BRAVE_API_KEY_ENV: &str = "BRAVE_API_KEY";

/// Everything this server reads from its host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InProcessConfig {
    /// This server's storage root (the download cache, search offloads).
    pub plugin_home: Option<PathBuf>,
    /// A Brave Search API key; empty when Brave is not configured.
    pub brave_api_key: String,
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
        Self {
            plugin_home,
            brave_api_key: non_empty(BRAVE_API_KEY_ENV).unwrap_or_default(),
        }
    }

    /// Build from this process's environment - the stdio binary's source.
    pub fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Build from an explicit map of the same variables - an in-process
    /// host's source. Nothing is read from the process environment except
    /// the OS home-directory fallback, so a key the host removed is gone.
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
        let map: HashMap<String, String> = [
            (PLUGIN_HOME_ENV.to_string(), "home".to_string()),
            (BRAVE_API_KEY_ENV.to_string(), "k".to_string()),
        ]
        .into();
        assert_eq!(
            InProcessConfig::from_map(&map),
            InProcessConfig {
                plugin_home: Some(PathBuf::from("home")),
                brave_api_key: "k".to_string(),
            }
        );
    }

    /// A key the host dropped from the map is gone, whatever the process
    /// environment holds - the Brave toggle bug.
    #[test]
    fn a_missing_key_means_no_brave() {
        assert!(InProcessConfig::from_map(&HashMap::new())
            .brave_api_key
            .is_empty());
    }

    #[tokio::test]
    async fn the_scoped_value_reaches_blocking_threads() {
        let config = Arc::new(InProcessConfig {
            brave_api_key: "scoped".to_string(),
            ..Default::default()
        });
        let seen = scope(config, async {
            (
                current().brave_api_key.clone(),
                spawn_blocking(|| current().brave_api_key.clone())
                    .await
                    .unwrap(),
            )
        })
        .await;
        assert_eq!(seen, ("scoped".to_string(), "scoped".to_string()));
    }
}
