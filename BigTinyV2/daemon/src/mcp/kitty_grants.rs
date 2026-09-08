//! The filesystem grant set handed to Kitty's bundled stdio tool servers.
//!
//! `kitty-tools` and `kitty-wasm` enforce a path boundary of their own, as
//! defense-in-depth behind this daemon's per-session `check_containment`. That
//! boundary used to be a single "home" directory resolved from
//! `KITTY_PLUGIN_HOME` — which is also where those servers keep their
//! scratchpad and document cache. Once `mcp::manager::scoped_env` began handing
//! each app its own `apps/<id>/plugin-home` so two apps could not share a
//! scratchpad, that per-app storage folder silently became the only tree the
//! file tools would read.
//!
//! The result was a model that could not open a file the user had just handed
//! it: the system prompt named the session's chat directory,
//! `allowed_dirs_for_session` allowed it, and every reader answered
//! `PATH_OUTSIDE_HOME` because it was not under the app's storage root.
//!
//! This module separates the two concerns. `KITTY_PLUGIN_HOME` keeps its
//! narrow, per-app meaning and governs storage only. Authorization travels
//! separately, split by how often it changes:
//!
//! * **Static** ([`static_allowed_dirs`]) — the user's home directory, the OS
//!   temp directory and the daemon's data root. Fixed for the life of the child
//!   process, so it rides in the environment at spawn.
//! * **Volatile** ([`publish`]) — the files the user attached to a turn and the
//!   working folders they chose. These are per-session and accumulate mid-run,
//!   and a stdio server is one long-lived process shared by every session whose
//!   environment is fixed at spawn, so they cannot ride there. The daemon
//!   writes them to a small JSON file the child re-reads when it changes.
//!
//! Both halves only ever *widen* the child's own second gate. This daemon has
//! already run the authoritative per-session check by the time a tool executes,
//! so a stale or missing grants file costs a spurious rejection, never an
//! unauthorized read.

use std::path::{Path, PathBuf};

/// Environment variable carrying [`static_allowed_dirs`]. Mirrors
/// `plugins/kitty-tools/src/paths.rs::ALLOWED_DIRS_ENV`.
pub const ALLOWED_DIRS_ENV: &str = "KITTY_ALLOWED_DIRS";

/// Environment variable naming the file [`publish`] writes. Mirrors
/// `plugins/kitty-tools/src/paths.rs::ALLOWED_DIRS_FILE_ENV`.
pub const ALLOWED_DIRS_FILE_ENV: &str = "KITTY_ALLOWED_DIRS_FILE";

/// The directories every session may reach, joined with the platform's `PATH`
/// separator so the child can parse them with `std::env::split_paths` — a plain
/// split on `:` would cut a Windows path at its drive letter.
pub fn static_allowed_dirs(data_dir: &Path) -> String {
    let mut dirs: Vec<PathBuf> = Vec::new();
    // (1) The home directory. The user's own files are the entire point of the
    //     file tools; anything narrower leaves them unable to read what the
    //     user pointed them at.
    dirs.push(crate::env_contract::dirs_home());
    // (2) Cache and temp. Tool scratch space, scraped pages and the
    //     extract-once document cache live here, and a tool that cannot read
    //     back its own cache re-does the extraction on every call.
    dirs.push(std::env::temp_dir());
    if !data_dir.as_os_str().is_empty() {
        dirs.push(data_dir.to_path_buf());
    }
    dirs.retain(|d| !d.as_os_str().is_empty());
    dirs.sort();
    dirs.dedup();
    std::env::join_paths(dirs)
        .map(|s| s.to_string_lossy().into_owned())
        // `join_paths` fails only when a path contains the separator itself,
        // which a real directory cannot on either platform. An empty value
        // leaves the child with its own home as the sole root — its prior
        // behaviour, and still fail-closed.
        .unwrap_or_default()
}

/// Where an app's grants file lives — one per app, alongside its plugin home.
pub fn grants_file(data_dir: &Path, app_id: &str) -> PathBuf {
    data_dir.join("apps").join(app_id).join("allowed-dirs.json")
}

/// Write the volatile grants for `app_id`.
///
/// `dirs` is a union across the app's sessions rather than one session's set,
/// because the file is process-wide and the child cannot tell which session a
/// call belongs to. That is sound for the reason given in the module docs: this
/// daemon has already applied the per-session check, and this file only stops
/// the child's second gate from contradicting the first.
///
/// Best-effort and idempotent: an unchanged set is not rewritten, so a
/// long-running turn does not churn the file the child stats on every call.
pub fn publish(data_dir: &Path, app_id: &str, dirs: &[String]) {
    if data_dir.as_os_str().is_empty() || app_id.is_empty() {
        return;
    }
    let mut sorted: Vec<&str> = dirs
        .iter()
        .map(|s| s.as_str())
        .filter(|s| !s.trim().is_empty())
        .collect();
    sorted.sort_unstable();
    sorted.dedup();

    let Ok(json) = serde_json::to_string(&sorted) else {
        return;
    };
    let path = grants_file(data_dir, app_id);
    // Skip a no-op write: `kitty-tools` caches on (mtime, len), so rewriting
    // identical content would drop its cache on every turn for nothing.
    if let Ok(existing) = std::fs::read_to_string(&path) {
        if existing == json {
            return;
        }
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, json);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_static_set_carries_home_and_temp() {
        let joined = static_allowed_dirs(Path::new(""));
        let roots: Vec<PathBuf> = std::env::split_paths(&joined).collect();
        assert!(roots.contains(&crate::env_contract::dirs_home()));
        assert!(roots.contains(&std::env::temp_dir()));
    }

    /// The regression this module exists to prevent: a chat directory under the
    /// user's home must fall inside the static set, because that is the path
    /// the system prompt tells the model its attached files live in.
    #[test]
    fn a_chat_directory_under_home_is_covered() {
        let joined = static_allowed_dirs(Path::new(""));
        let roots: Vec<PathBuf> = std::env::split_paths(&joined).collect();
        let chat = crate::env_contract::dirs_home()
            .join("Documents")
            .join("Kitty")
            .join("chats")
            .join("20260908_104607-686d64");
        assert!(
            roots.iter().any(|r| chat.starts_with(r)),
            "no static root contains {chat:?}"
        );
    }

    #[test]
    fn publishing_is_idempotent_and_sorted() {
        let tmp = std::env::temp_dir().join(format!("kg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        publish(
            &tmp,
            "app1",
            &["/b".into(), "/a".into(), "  ".into(), "/a".into()],
        );
        let path = grants_file(&tmp, "app1");
        let written = std::fs::read_to_string(&path).expect("grants written");
        assert_eq!(written, "[\"/a\",\"/b\"]");

        // A second identical publish must not touch the file — see `publish`.
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        publish(&tmp, "app1", &["/a".into(), "/b".into()]);
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, after);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn an_empty_data_dir_or_app_writes_nothing() {
        publish(Path::new(""), "app1", &["/a".into()]);
        publish(&std::env::temp_dir(), "", &["/a".into()]);
        assert!(!grants_file(Path::new(""), "app1").exists());
    }
}
