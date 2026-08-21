//! Where this crate's two on-disk stores live.
//!
//! `scrape` caches downloaded PDFs and `search` offloads large result sets;
//! both were derived straight from `dirs::home_dir()`, with `"."` as the
//! fallback. That works on desktop and fails completely on Android, where the
//! app process has no useful `$HOME` (bionic's `getpwuid` reports `/data`) and
//! a working directory of `/`. Both stores landed under the filesystem root:
//! `create_dir_all` failed, the PDF cache reported the error, and — worse —
//! the search offload swallowed it and still handed the model a `search_id`
//! that `lean_web_search_read_chunk` could never resolve.
//!
//! Deliberately duplicated from `kitty-tools`/`kitty-wasm` rather than shared
//! through a common crate: these ship as separate frozen binaries, and a
//! cross-crate dependency for one resolver would drag the wrong way. Same
//! reasoning as `envelope.rs`'s duplication note.

use std::path::PathBuf;
use std::sync::OnceLock;

/// The environment variable a host sets to tell this crate where the user's
/// files actually live. Kitty sets it to its resolved app data directory
/// before the daemon starts (`src-tauri/src/lifecycle/bigtiny_env.rs`), and
/// only on Android; desktop resolution is unchanged.
pub const PLUGIN_HOME_ENV: &str = "KITTY_PLUGIN_HOME";

/// The user's home directory, resolved once per process, or `None` when it
/// genuinely cannot be determined.
pub fn home_dir() -> Option<PathBuf> {
    static HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
    HOME.get_or_init(|| resolve_home(|key| std::env::var(key).ok()))
        .clone()
}

/// The resolution order itself, taking its environment as a parameter so it
/// is testable without mutating the process (and without fighting
/// `home_dir`'s process-lifetime cache).
fn resolve_home(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    [PLUGIN_HOME_ENV, "USERPROFILE", "HOME"]
        .into_iter()
        .find_map(|key| env(key).filter(|p| !p.trim().is_empty()).map(PathBuf::from))
        .or_else(dirs::home_dir)
}

/// The base both stores hang off. `None` home yields a relative placeholder,
/// which is not a usable location — callers surface the resulting write
/// failure rather than silently carrying on (see `search::write_offload`).
fn base_dir() -> PathBuf {
    home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Same cache directory the Python tools use, so a PDF downloaded by
/// `lean_web_scrape` stays readable by `lean_pdf_read_text` and inspectable
/// via `kitty-tools`' `lean_cache_view` without the two halves having to
/// agree on a second location. Byte-identical to `kitty-tools`'
/// `tools::cache_dir()`; keep them in step.
pub fn cache_dir() -> PathBuf {
    base_dir().join(".cache").join("lean-goose-mcp")
}

/// Sibling to the tool cache dir, not inside it — so a future cache-clear
/// tool can never delete an in-flight search offload. Mirrors the Python
/// constant `SEARCH_STORE_DIR` (`~/.cache/kitty-search-offload`) exactly, so
/// a mixed Python/Rust install shares one store.
pub fn search_store_dir() -> PathBuf {
    base_dir().join(".cache").join("kitty-search-offload")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plugin_home_override_wins_over_the_usual_sources() {
        let env = |key: &str| match key {
            PLUGIN_HOME_ENV => Some("/data/user/0/com.kitty/files".to_string()),
            "HOME" => Some("/".to_string()),
            _ => None,
        };
        assert_eq!(
            resolve_home(env),
            Some(PathBuf::from("/data/user/0/com.kitty/files"))
        );
    }

    /// An empty or whitespace-only value is not a home directory.
    #[test]
    fn blank_environment_values_are_skipped_not_used() {
        let env = |key: &str| match key {
            PLUGIN_HOME_ENV => Some("".to_string()),
            "USERPROFILE" => Some("   ".to_string()),
            "HOME" => Some("/home/real".to_string()),
            _ => None,
        };
        assert_eq!(resolve_home(env), Some(PathBuf::from("/home/real")));
    }

    /// No working-directory fallback: on an Android app process that is `/`,
    /// and both stores would be created (or fail to be) at the filesystem
    /// root.
    #[test]
    fn resolution_never_falls_back_to_the_working_directory() {
        let resolved = resolve_home(|_| None);
        assert_eq!(resolved, dirs::home_dir());
        if let Ok(cwd) = std::env::current_dir() {
            assert_ne!(resolved, Some(cwd));
        }
    }

    /// The two stores must stay siblings — `search_store_dir` is deliberately
    /// *outside* `cache_dir` so a cache clear cannot take the offloads with
    /// it.
    #[test]
    fn the_offload_store_is_not_inside_the_cache_dir() {
        assert!(!search_store_dir().starts_with(cache_dir()));
        assert_eq!(search_store_dir().parent(), cache_dir().parent());
    }
}
