//! Path containment for the `workspace` mount (audit #111).
//!
//! The `workspace` tool argument mounts a host directory **read-write** into
//! the guest at `/work`. Before this check it accepted *any* existing
//! directory — `C:\`, `C:\Windows\System32`, another user's profile — giving
//! model-written guest code read-write run of the host. The policy here is
//! the same one `kitty-tools` applies to every file tool (`paths.rs` there,
//! ported rather than shared: these ship as separate frozen binaries, and a
//! cross-crate dependency for one function would drag the wrong way — see
//! `envelope.rs`'s duplication note in kitty-web for the same reasoning):
//! the workspace must resolve to a location inside the user's home
//! directory. The daemon is the primary path-containment gate; this is
//! defense-in-depth for the crate's own tool surface.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The environment variable a host sets to tell this crate where the user's
/// files actually live. See `kitty_tools::paths::PLUGIN_HOME_ENV` — same
/// variable, same reason, duplicated for the same reason the rest of this
/// module is (these ship as separate frozen binaries).
pub const PLUGIN_HOME_ENV: &str = "KITTY_PLUGIN_HOME";

/// The user's home directory, resolved once per process, or `None` when it
/// genuinely cannot be determined. `KITTY_PLUGIN_HOME` wins, then
/// `%USERPROFILE%` (Windows) or `$HOME`, then `dirs::home_dir`.
///
/// **There is deliberately no working-directory fallback.** There used to be,
/// and it silently inverted the boundary: on a host where nothing else
/// resolves, the working directory can be `/` (an Android app process), so
/// `path_within_home` compared every path against the filesystem root and
/// answered `true` for all of them — meaning `workspace` could mount *any*
/// directory on the device read-write into the guest, which is exactly what
/// audit #111 added this check to prevent.
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

/// True when `path` resolves to a location inside the user's home
/// directory. Mirrors kitty-tools' `path_within_home`: canonicalize the
/// nearest existing ancestor (the workspace itself always exists — callers
/// check `is_dir` first) so symlinked components and Windows 8.3 short-name
/// segments can't alias their way across the boundary, with a lexical
/// fallback for paths that don't exist yet.
/// An undeterminable home directory rejects everything — see `home_dir`.
pub fn path_within_home(path: &Path) -> bool {
    within_home_of(home_dir().as_deref(), path)
}

/// The containment test against an explicit home, split out so the
/// fail-closed behaviour can be tested directly rather than by trying to
/// convince the process it has no home directory.
fn within_home_of(home: Option<&Path>, path: &Path) -> bool {
    // No home means no boundary to be inside of. Fail closed.
    let Some(home) = home else {
        return false;
    };
    let home_canon = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());

    if let Some(anchor) = nearest_existing_ancestor(path) {
        if let Ok(canon) = std::fs::canonicalize(&anchor) {
            return path_is_within(&home_canon, &canon);
        }
    }

    path_is_within(&home_canon, path)
}

/// The deepest ancestor of `path` (including `path` itself) that exists on
/// disk, or `None` if even the root-most segment is unavailable.
fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        if current.exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Case-insensitive (on Windows) "is `candidate` equal to `base` or strictly
/// inside it" check; on Windows the boundary is a trailing `\` so a sibling
/// like `C:\Users\alice2` can't alias `C:\Users\alice`.
#[cfg(windows)]
fn path_is_within(base: &Path, candidate: &Path) -> bool {
    fn norm(p: &Path) -> String {
        p.to_string_lossy().replace('/', "\\").to_lowercase()
    }
    let base_s = norm(base);
    let candidate_s = norm(candidate);
    candidate_s == base_s || candidate_s.starts_with(&format!("{base_s}\\"))
}

#[cfg(not(windows))]
fn path_is_within(base: &Path, candidate: &Path) -> bool {
    candidate == base || candidate.starts_with(base.as_os_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        home_dir().expect("the test host has a home directory")
    }

    #[test]
    fn home_dir_is_inside_itself() {
        assert!(path_within_home(&home()));
    }

    #[test]
    fn paths_inside_home_are_allowed() {
        let within = home().join("some").join("deeper").join("workspace");
        assert!(path_within_home(&within));
        // Case differences must not slip past the boundary on Windows.
        assert!(path_within_home(&home().join("MixedCase").join("x")));
    }

    /// The regression that matters most here: an undeterminable home used to
    /// fall back to the working directory, which on an Android app process is
    /// `/`. Every path then counted as "inside home", so `validate_workspace`
    /// would have happily mounted any directory on the device read-write into
    /// the guest — the exact hole audit #111 closed.
    #[test]
    fn an_undeterminable_home_rejects_every_workspace() {
        for candidate in [
            "/",
            "/etc",
            "/data/user/0/com.example/files",
            r"C:\Windows\system32",
        ] {
            assert!(
                !within_home_of(None, Path::new(candidate)),
                "{candidate} must be rejected when there is no home to be inside of"
            );
        }
    }

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

    #[test]
    fn resolution_never_falls_back_to_the_working_directory() {
        let resolved = resolve_home(|_| None);
        assert_eq!(resolved, dirs::home_dir());
        if let Ok(cwd) = std::env::current_dir() {
            assert_ne!(
                resolved,
                Some(cwd),
                "the working directory must never stand in for a home directory"
            );
        }
    }

    #[test]
    fn absolute_paths_outside_home_are_rejected() {
        #[cfg(windows)]
        let outside = PathBuf::from("C:\\Windows\\system32");
        #[cfg(not(windows))]
        let outside = PathBuf::from("/etc");
        assert!(!path_within_home(&outside));
    }

    #[test]
    fn sibling_directory_does_not_count_as_home() {
        // `C:\Users\alice2` must not be treated as inside `C:\Users\alice`.
        let base = home();
        let mut sibling = base.clone();
        if let Some(name) = base.file_name().map(|n| n.to_string_lossy().into_owned()) {
            sibling = base.parent().unwrap().join(format!("{name}2"));
        }
        if sibling != base {
            assert!(!path_within_home(&sibling));
        }
    }
}
